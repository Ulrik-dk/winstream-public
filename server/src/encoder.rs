use std::{
    collections::HashMap,
    ffi::c_void,
    sync::{Arc, mpsc},
    thread::{self, JoinHandle},
};

use anyhow::{Context, Result, anyhow};
use cudarc::driver::CudaContext;
use nvidia_video_codec_sdk::{
    Bitstream, Buffer, EncodePictureParams, Encoder, EncoderInitParams, EncoderInput,
    RegisteredResource, Session,
    sys::nvEncodeAPI::{
        NV_ENC_BUFFER_FORMAT::NV_ENC_BUFFER_FORMAT_ABGR, NV_ENC_CODEC_H264_GUID, NV_ENC_CONFIG,
        NV_ENC_INPUT_RESOURCE_TYPE, NV_ENC_PARAMS_RC_MODE, NV_ENC_PIC_TYPE, NV_ENC_PRESET_P1_GUID,
        NV_ENC_TUNING_INFO,
    },
};
use proto::FrameHeader;
use tokio::sync::mpsc::UnboundedSender;
use tracing::{error, info, warn};

pub mod rate;

use crate::{
    clock,
    gpu_copy::{DeviceBuffer, SlotGuard},
};

#[derive(Clone, Copy, Debug)]
pub struct Settings {
    pub fps: u32,
    pub latency: std::time::Duration,
    pub start_bpp: f64,
    pub max_bpp: f64,
    pub min_bitrate: u32,
}

impl Settings {
    pub fn frame_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs_f64(1.0 / self.fps as f64)
    }
}
const TUNING: NV_ENC_TUNING_INFO = NV_ENC_TUNING_INFO::NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
const RETRY: std::time::Duration = std::time::Duration::from_millis(500);

pub struct RawFrame {
    pub picture: Picture,
    pub width: u32,
    pub height: u32,
    pub visible_width: u32,
    pub visible_height: u32,
    pub frame_id: u64,
    pub commit_ns: u64,
    pub sent_ns: u64,
}

pub enum Picture {
    Gpu(SlotGuard),
    Cpu(Vec<u8>),
}

pub enum EncoderCmd {
    Restart,
    Frame(RawFrame),
    Feedback {
        received: u32,
        shown: u32,
        last_frame_id: u64,
        latency: std::time::Duration,
    },
}

pub struct EncodedFrame {
    pub header: FrameHeader,
    pub data: Vec<u8>,
}

pub struct EncoderHandle {
    pub tx: mpsc::Sender<EncoderCmd>,
    thread: JoinHandle<()>,
}

impl EncoderHandle {
    pub fn shutdown(self) {
        drop(self.tx);
        let _ = self.thread.join();
    }
}

pub fn spawn(
    window_id: u32,
    out: UnboundedSender<EncodedFrame>,
    settings: Settings,
) -> EncoderHandle {
    let (tx, rx) = mpsc::channel();
    let thread = thread::Builder::new()
        .name(format!("encoder-{window_id}"))
        .spawn(move || {
            if let Err(err) = run(window_id, rx, out, settings) {
                error!(window_id, "encoder stopped: {err:#}");
            }
        })
        .expect("spawn encoder thread");
    EncoderHandle { tx, thread }
}

struct Active {
    registered: HashMap<u64, RegisteredResource<'static, Arc<DeviceBuffer>>>,
    output: Bitstream<'static>,
    input: Buffer<'static>,
    session: Box<Session>,
    config: Box<NV_ENC_CONFIG>,
    width: u32,
    height: u32,
    pitch: usize,
    fps: u32,
    staging: Vec<u8>,
}

impl Active {
    fn open(
        cuda: &std::sync::Arc<CudaContext>,
        width: u32,
        height: u32,
        bitrate: u32,
        fps: u32,
    ) -> Result<Self> {
        let encoder = Encoder::initialize_with_cuda(cuda.clone())
            .map_err(|e| anyhow!("open NVENC: {e:?}"))?;
        let preset = encoder
            .get_preset_config(NV_ENC_CODEC_H264_GUID, NV_ENC_PRESET_P1_GUID, TUNING)
            .map_err(|e| anyhow!("preset: {e:?}"))?;
        let mut config = Box::new(preset.presetCfg);
        config.gopLength = u32::MAX;
        config.frameIntervalP = 1;
        config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_MODE::NV_ENC_PARAMS_RC_CBR;
        set_rate(&mut config, bitrate, fps);
        let mut params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, width, height);
        params
            .preset_guid(NV_ENC_PRESET_P1_GUID)
            .tuning_info(TUNING)
            .encode_config(&mut config)
            .framerate(fps, 1)
            .enable_picture_type_decision();
        let session = Box::new(
            encoder
                .start_session(NV_ENC_BUFFER_FORMAT_ABGR, params)
                .map_err(|e| anyhow!("start session {width}x{height}: {e:?}"))?,
        );
        let session_ref: &'static Session = unsafe { &*(session.as_ref() as *const Session) };
        let input = session_ref
            .create_input_buffer()
            .map_err(|e| anyhow!("input buffer: {e:?}"))?;
        let mut input = input;
        let output = session_ref
            .create_output_bitstream()
            .map_err(|e| anyhow!("bitstream: {e:?}"))?;
        drop(input.lock().map_err(|e| anyhow!("lock input: {e:?}"))?);
        let reported = input.pitch() as usize;
        let row = width as usize * 4;
        let pitch = if reported >= row {
            reported
        } else {
            reported * 4
        };
        info!(
            width,
            height,
            reported_pitch = reported,
            pitch,
            "NVENC input buffer"
        );
        Ok(Active {
            registered: HashMap::new(),
            output,
            input,
            session,
            config,
            width,
            height,
            pitch,
            fps,
            staging: Vec::new(),
        })
    }

    fn set_bitrate(&mut self, bitrate: u32) -> Result<()> {
        set_rate(&mut self.config, bitrate, self.fps);
        let mut params = EncoderInitParams::new(NV_ENC_CODEC_H264_GUID, self.width, self.height);
        params
            .preset_guid(NV_ENC_PRESET_P1_GUID)
            .tuning_info(TUNING)
            .encode_config(&mut self.config)
            .framerate(self.fps, 1)
            .enable_picture_type_decision();
        self.session
            .reconfigure(&mut params)
            .map_err(|e| anyhow!("change bitrate to {bitrate}: {e:?}"))
    }

    fn encode(&mut self, frame: &RawFrame, timing: &mut EncodeTiming) -> Result<(Vec<u8>, bool)> {
        let t0 = std::time::Instant::now();
        let params = EncodePictureParams {
            input_timestamp: frame.frame_id,
            ..Default::default()
        };
        let t1;
        match &frame.picture {
            Picture::Gpu(slot) => {
                let buffer = &slot.buffer;
                if !self.registered.contains_key(&buffer.ptr) {
                    let session: &'static Session =
                        unsafe { &*(self.session.as_ref() as *const Session) };
                    let resource = session
                        .register_generic_resource(
                            buffer.clone(),
                            NV_ENC_INPUT_RESOURCE_TYPE::NV_ENC_INPUT_RESOURCE_TYPE_CUDADEVICEPTR,
                            buffer.ptr as *mut c_void,
                            buffer.pitch as u32,
                        )
                        .map_err(|e| anyhow!("register CUDA buffer: {e:?}"))?;
                    self.registered.insert(buffer.ptr, resource);
                }
                t1 = std::time::Instant::now();
                let resource = self.registered.get_mut(&buffer.ptr).unwrap();
                self.session
                    .encode_picture(resource, &mut self.output, params)
                    .map_err(|e| anyhow!("encode: {e:?}"))?;
            }
            Picture::Cpu(pixels) => {
                let row = self.width as usize * 4;
                let data: &[u8] = if self.pitch == row {
                    pixels
                } else {
                    self.staging.resize(self.pitch * self.height as usize, 0);
                    for (dst, src) in self
                        .staging
                        .chunks_exact_mut(self.pitch)
                        .zip(pixels.chunks_exact(row))
                    {
                        dst[..row].copy_from_slice(src);
                    }
                    &self.staging
                };
                unsafe {
                    self.input
                        .lock()
                        .map_err(|e| anyhow!("lock input: {e:?}"))?
                        .write(data)
                };
                t1 = std::time::Instant::now();
                self.session
                    .encode_picture(&mut self.input, &mut self.output, params)
                    .map_err(|e| anyhow!("encode: {e:?}"))?;
            }
        }
        let t2 = std::time::Instant::now();
        let lock = self
            .output
            .lock()
            .map_err(|e| anyhow!("lock output: {e:?}"))?;
        let keyframe = lock.picture_type() == NV_ENC_PIC_TYPE::NV_ENC_PIC_TYPE_IDR;
        let data = lock.data().to_vec();
        let t3 = std::time::Instant::now();
        let us = |a: std::time::Instant, b: std::time::Instant| (b - a).as_micros() as u64;
        timing.frames += 1;
        timing.upload_us += us(t0, t1);
        timing.encode_us += us(t1, t2);
        timing.output_us += us(t2, t3);
        Ok((data, keyframe))
    }
}

fn set_rate(config: &mut NV_ENC_CONFIG, bitrate: u32, fps: u32) {
    config.rcParams.averageBitRate = bitrate;
    config.rcParams.vbvBufferSize = bitrate / fps;
    config.rcParams.vbvInitialDelay = bitrate / fps;
}

#[derive(Default)]
struct EncodeTiming {
    frames: u32,
    queue_us: u64,
    upload_us: u64,
    encode_us: u64,
    output_us: u64,
}

fn run(
    window_id: u32,
    rx: mpsc::Receiver<EncoderCmd>,
    out: UnboundedSender<EncodedFrame>,
    settings: Settings,
) -> Result<()> {
    let cuda = CudaContext::new(0).context("CUDA context")?;
    let mut timing = EncodeTiming::default();
    let mut timing_since = std::time::Instant::now();
    let mut active: Option<Active> = None;
    let mut retry_at: Option<std::time::Instant> = None;
    let mut failures = 0u32;
    let mut skipped = 0u64;
    let mut last_report = std::time::Instant::now();
    let mut rate: Option<rate::RateControl> = None;
    let mut changes: Option<(u32, u32)> = None;
    let mut changes_logged = std::time::Instant::now();
    let mut backlog_skips = 0u64;
    while let Ok(first) = rx.recv() {
        let mut frame = None;
        for cmd in std::iter::once(first).chain(rx.try_iter()) {
            match cmd {
                EncoderCmd::Restart => {
                    active = None;
                    rate = None;
                }
                EncoderCmd::Frame(f) => {
                    if frame.replace(f).is_some() {
                        skipped += 1;
                    }
                }
                EncoderCmd::Feedback {
                    received,
                    shown,
                    last_frame_id,
                    latency,
                } => {
                    let Some(rate) = &mut rate else { continue };
                    let from = rate.bitrate();
                    let Some(to) = rate.on_feedback(
                        received,
                        shown,
                        last_frame_id,
                        latency,
                        std::time::Instant::now(),
                    ) else {
                        continue;
                    };
                    let entry = changes.get_or_insert((from, 0));
                    entry.1 += 1;
                    if let Some(a) = &mut active
                        && let Err(err) = a.set_bitrate(to)
                    {
                        warn!(window_id, "{err:#}; starting a new session");
                        active = None;
                    }
                }
            }
        }
        if let (Some((from, steps)), Some(rate)) = (changes, &rate)
            && changes_logged.elapsed().as_secs() >= 1
        {
            info!(
                window_id,
                from = format!("{:.1}", from as f64 / 1e6),
                to = format!("{:.1}", rate.bitrate() as f64 / 1e6),
                steps,
                backlog_ms = rate.backlog().as_millis() as u64,
                latency_ms = rate.latency().as_millis() as u64,
                backlog_skips,
                "bitrate (Mbit/s)"
            );
            changes = None;
            backlog_skips = 0;
            changes_logged = std::time::Instant::now();
        }
        let Some(frame) = frame else {
            continue;
        };
        if skipped > 0 && last_report.elapsed().as_secs() >= 1 {
            info!(window_id, skipped, "encoder behind: skipped stale frames");
            skipped = 0;
            last_report = std::time::Instant::now();
        }
        if active
            .as_ref()
            .is_some_and(|a| (a.width, a.height) != (frame.width, frame.height))
        {
            active = None;
            if let Some(rate) = &mut rate {
                rate.resize(frame.width, frame.height);
            }
        }
        if rate
            .as_mut()
            .is_some_and(|r| r.should_skip(std::time::Instant::now()))
        {
            backlog_skips += 1;
            continue;
        }
        if retry_at.is_some_and(|t| std::time::Instant::now() < t) {
            continue;
        }
        let rate =
            rate.get_or_insert_with(|| rate::RateControl::new(frame.width, frame.height, settings));
        let encoded = (|| {
            if active.is_none() {
                let bitrate = rate.bitrate();
                active = Some(Active::open(
                    &cuda,
                    frame.width,
                    frame.height,
                    bitrate,
                    settings.fps,
                )?);
                info!(
                    window_id,
                    width = frame.width,
                    height = frame.height,
                    mbit = bitrate / 1_000_000,
                    "NVENC session started"
                );
            }
            timing.queue_us += clock::now_ns().saturating_sub(frame.sent_ns) / 1000;
            active.as_mut().unwrap().encode(&frame, &mut timing)
        })();
        let (data, keyframe) = match encoded {
            Ok(done) => {
                if retry_at.take().is_some() {
                    info!(window_id, failures, "encoder works again");
                    failures = 0;
                }
                done
            }
            Err(err) => {
                active = None;
                retry_at = Some(std::time::Instant::now() + RETRY);
                failures += 1;
                if failures == 1 || failures.is_multiple_of(20) {
                    error!(
                        window_id,
                        failures, "encoder failed, trying again in {RETRY:?}: {err:#}"
                    );
                }
                continue;
            }
        };
        if timing_since.elapsed().as_secs() >= 1 && timing.frames > 0 {
            let n = timing.frames as f64;
            let ms = |us: u64| format!("{:.2}", us as f64 / n / 1000.0);
            info!(
                window_id,
                frames = timing.frames,
                queue_ms = ms(timing.queue_us),
                upload_ms = ms(timing.upload_us),
                encode_ms = ms(timing.encode_us),
                output_ms = ms(timing.output_us),
                mbit_target = format!("{:.1}", rate.bitrate() as f64 / 1e6),
                "encoder"
            );
            timing = EncodeTiming::default();
            timing_since = std::time::Instant::now();
        }
        let header = FrameHeader {
            window_id,
            frame_id: frame.frame_id,
            commit_ns: frame.commit_ns,
            encoded_ns: clock::now_ns(),
            width: frame.width as u16,
            height: frame.height as u16,
            visible_width: frame.visible_width as u16,
            visible_height: frame.visible_height as u16,
            keyframe,
            len: data.len() as u32,
        };
        if out.send(EncodedFrame { header, data }).is_err() {
            break;
        }
        rate.on_sent(frame.frame_id, std::time::Instant::now());
    }
    Ok(())
}
