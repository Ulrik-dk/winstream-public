use std::{process::Command, thread};

use anyhow::{Context, Result, bail};
use libpulse_binding::{
    def::BufferAttr,
    sample::{Format, Spec},
    stream::Direction,
};
use libpulse_simple_binding::Simple;
use proto::{AUDIO_CHANNELS, AUDIO_FRAME_SAMPLES, AUDIO_RATE, AudioHeader};
use tokio::sync::mpsc::Sender;
use tracing::{error, info, warn};

use crate::clock;

const BITRATE: i32 = 128_000;

pub struct Sink {
    pub name: String,
    module: String,
    previous_default: Option<String>,
}

impl Sink {
    pub fn create() -> Result<Self> {
        remove_stale_sinks();
        let name = format!("winstream_{}", std::process::id());
        let previous_default = pactl(&["get-default-sink"])
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.starts_with("winstream_"));
        let module = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={name}"),
            "sink_properties=device.description=winstream",
        ])
        .context("creating the null sink (is PipeWire/PulseAudio running?)")?
        .trim()
        .to_string();
        if let Err(err) = pactl(&["set-default-sink", &name]) {
            warn!("could not make {name} the default sink: {err:#}");
        }
        info!(%name, %module, "audio sink created");
        Ok(Sink {
            name,
            module,
            previous_default,
        })
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        if let Some(previous) = &self.previous_default
            && previous != &self.name
        {
            let _ = pactl(&["set-default-sink", previous]);
        }
        let _ = pactl(&["unload-module", &self.module]);
    }
}

fn remove_stale_sinks() {
    let Ok(modules) = pactl(&["list", "short", "modules"]) else {
        return;
    };
    for line in modules.lines() {
        let mut fields = line.split_whitespace();
        let (Some(index), Some("module-null-sink"), Some(args)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let Some(pid) = args.strip_prefix("sink_name=winstream_") else {
            continue;
        };
        if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
            info!(sink = %args, "removing stale audio sink");
            let _ = pactl(&["unload-module", index]);
        }
    }
}

fn pactl(args: &[&str]) -> Result<String> {
    let out = Command::new("pactl")
        .args(args)
        .output()
        .context("running pactl")?;
    if !out.status.success() {
        bail!(
            "pactl {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn spawn_capture(sink: &str, out: Sender<Vec<u8>>) {
    let source = format!("{sink}.monitor");
    thread::Builder::new()
        .name("audio".into())
        .spawn(move || {
            if let Err(err) = capture(&source, out) {
                error!("audio capture stopped: {err:#}");
            }
        })
        .expect("spawn audio thread");
}

fn capture(source: &str, out: Sender<Vec<u8>>) -> Result<()> {
    let spec = Spec {
        format: Format::S16le,
        channels: AUDIO_CHANNELS as u8,
        rate: AUDIO_RATE,
    };
    let chunk_bytes = AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS * 2;
    let attr = BufferAttr {
        maxlength: u32::MAX,
        tlength: u32::MAX,
        prebuf: u32::MAX,
        minreq: u32::MAX,
        fragsize: chunk_bytes as u32,
    };
    let record = Simple::new(
        None,
        "winstream",
        Direction::Record,
        Some(source),
        "app audio",
        &spec,
        None,
        Some(&attr),
    )
    .map_err(|e| anyhow::anyhow!("recording {source}: {e}"))?;

    let mut encoder = proto::opus::Encoder::new(BITRATE)?;
    info!(%source, "audio capture started");

    let mut pcm_bytes = vec![0u8; chunk_bytes];
    let mut pcm = vec![0i16; AUDIO_FRAME_SAMPLES * AUDIO_CHANNELS];
    let mut opus_buf = vec![0u8; 4000];
    let mut seq = 0u32;
    let (mut latency_sum_us, mut latency_n, mut since) = (0u64, 0u64, std::time::Instant::now());
    loop {
        record
            .read(&mut pcm_bytes)
            .map_err(|e| anyhow::anyhow!("reading audio: {e}"))?;
        let capture_ns = clock::now_ns();
        if let Ok(latency) = record.get_latency() {
            latency_sum_us += latency.0;
            latency_n += 1;
        }
        if since.elapsed().as_secs() >= 1 && latency_n > 0 {
            info!(
                latency_ms = format!("{:.2}", latency_sum_us as f64 / latency_n as f64 / 1000.0),
                "audio capture"
            );
            (latency_sum_us, latency_n, since) = (0, 0, std::time::Instant::now());
        }
        for (s, b) in pcm.iter_mut().zip(pcm_bytes.as_chunks::<2>().0) {
            *s = i16::from_le_bytes(*b);
        }
        let len = encoder.encode(&pcm, &mut opus_buf)?;
        let mut packet = Vec::with_capacity(AudioHeader::LEN + len);
        packet.extend_from_slice(&AudioHeader { seq, capture_ns }.encode());
        packet.extend_from_slice(&opus_buf[..len]);
        seq = seq.wrapping_add(1);
        if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = out.try_send(packet) {
            return Ok(());
        }
    }
}
