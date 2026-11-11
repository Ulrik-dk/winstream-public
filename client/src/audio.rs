use std::{
    collections::VecDeque,
    sync::{Arc, atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use bytes::Bytes;
use libpulse_binding::{
    def::BufferAttr,
    sample::{Format, Spec},
    stream::Direction,
};
use libpulse_simple_binding::Simple;
use proto::{
    AUDIO_CHANNELS, AUDIO_FRAME_MS, AUDIO_FRAME_SAMPLES, AUDIO_RATE, AudioHeader, opus::Decoder,
};
use tracing::{error, info, warn};

use crate::{clock, net::ClockSync};

const MAX_QUEUED: usize = 2;
const GAP: Duration = Duration::from_millis(40);
const IDLE: Duration = Duration::from_millis(150);
const MAX_CONCEAL: u32 = 5;

pub fn spawn(packets: mpsc::Receiver<Bytes>, sync: Arc<ClockSync>) {
    thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(err) = run(packets, &sync) {
                error!("audio stopped: {err:#}");
            }
        })
        .expect("spawn audio thread");
}

struct Output {
    stream: Simple,
    bytes: Vec<u8>,
}

impl Output {
    fn open() -> Result<Self> {
        let frame_bytes = (AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS * 2) as u32;
        let spec = Spec {
            format: Format::S16le,
            channels: AUDIO_CHANNELS as u8,
            rate: AUDIO_RATE,
        };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: 3 * frame_bytes,
            prebuf: 2 * frame_bytes,
            minreq: frame_bytes,
            fragsize: u32::MAX,
        };
        let stream = Simple::new(
            None,
            "winstream",
            Direction::Playback,
            None,
            "remote app audio",
            &spec,
            None,
            Some(&attr),
        )
        .map_err(|e| anyhow::anyhow!("opening audio output: {e}"))?;
        info!("audio playback started");
        Ok(Output {
            stream,
            bytes: vec![0u8; frame_bytes as usize],
        })
    }

    fn buffered_us(&self) -> u64 {
        self.stream.get_latency().map_or(0, |l| l.0)
    }

    fn write(&mut self, pcm: &[i16]) -> Result<()> {
        for (b, s) in self.bytes.as_chunks_mut::<2>().0.iter_mut().zip(pcm) {
            *b = s.to_le_bytes();
        }
        self.stream
            .write(&self.bytes)
            .map_err(|e| anyhow::anyhow!("audio output: {e}"))
    }
}

fn run(packets: mpsc::Receiver<Bytes>, sync: &ClockSync) -> Result<()> {
    let mut output: Option<Output> = None;
    let mut decoder = Decoder::new()?;
    let mut pcm = vec![0i16; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS];
    let mut queue: VecDeque<Bytes> = VecDeque::new();
    let mut expected: Option<u32> = None;
    let mut concealed_ahead = 0u32;
    let mut last_packet = Instant::now();
    let mut last_write = Instant::now();
    let (mut dropped, mut concealed, mut report) = (0u32, 0u32, Instant::now());

    loop {
        if queue.is_empty() {
            match packets.recv_timeout(Duration::from_millis(AUDIO_FRAME_MS as u64)) {
                Ok(p) => queue.push_back(p),
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(out) = &mut output
                        && last_packet.elapsed() < IDLE
                        && last_write.elapsed() >= GAP
                        && decoder.decode(None, &mut pcm).is_ok()
                    {
                        out.write(&pcm)?;
                        last_write = Instant::now();
                        concealed_ahead += 1;
                        concealed += 1;
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        queue.extend(packets.try_iter());
        let packet = queue.pop_front().unwrap();
        let Some(header) = AudioHeader::decode(&packet) else {
            continue;
        };
        last_packet = Instant::now();
        if output.is_none() {
            output = Some(Output::open()?);
        }
        let out = output.as_mut().unwrap();

        if let Some(exp) = expected {
            let lost = header.seq.wrapping_sub(exp);
            if lost > 0 && lost < 1000 {
                let need = lost.saturating_sub(concealed_ahead).min(MAX_CONCEAL);
                for _ in 0..need {
                    if decoder.decode(None, &mut pcm).is_ok() {
                        out.write(&pcm)?;
                        concealed += 1;
                    }
                }
                concealed_ahead = concealed_ahead.saturating_sub(lost);
            }
        }
        expected = Some(header.seq.wrapping_add(1));

        match decoder.decode(Some(&packet[AudioHeader::LEN..]), &mut pcm) {
            Ok(_) => {
                if concealed_ahead > 0 {
                    concealed_ahead -= 1;
                    dropped += 1;
                } else if queue.len() > MAX_QUEUED {
                    dropped += 1;
                } else {
                    out.write(&pcm)?;
                    last_write = Instant::now();
                }
            }
            Err(err) => warn!("audio decode: {err}"),
        }

        let offset = sync.offset_ns.load(Ordering::Relaxed);
        let captured = header.capture_ns as i64 - offset;
        let to_output_us = (clock::now_ns() as i64 - captured) / 1000;
        let output_us = out.buffered_us() as i64;
        let delay_us = to_output_us + output_us;
        sync.audio_delay_us.store(delay_us, Ordering::Relaxed);
        let t = &sync.audio;
        t.packets.fetch_add(1, Ordering::Relaxed);
        t.to_output_us.fetch_add(to_output_us, Ordering::Relaxed);
        t.output_us.fetch_add(output_us, Ordering::Relaxed);

        if report.elapsed() >= Duration::from_secs(1) {
            sync.audio
                .concealed
                .fetch_add(concealed as i64, Ordering::Relaxed);
            sync.audio
                .dropped
                .fetch_add(dropped as i64, Ordering::Relaxed);
            if dropped > 0 || concealed > 0 {
                info!(concealed, dropped, delay_ms = delay_us / 1000, "audio");
            }
            (dropped, concealed, report) = (0, 0, Instant::now());
        }
    }
    Ok(())
}
