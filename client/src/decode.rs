use std::{
    ptr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
};

use anyhow::{Context, Result, anyhow, bail};
use ffmpeg_next::{
    self as ff, codec, ffi,
    format::Pixel,
    software::scaling::{self, flag::Flags},
};
use tracing::{debug, error, warn};
use winit::event_loop::EventLoopProxy;

use crate::{Decoded, Latest, Picture, UserEvent, clock, net::Received};

pub static GPU_PATH: AtomicBool = AtomicBool::new(true);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColorInfo {
    pub bt709: bool,
    pub full_range: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct Plane {
    pub fourcc: u32,
    pub fd: i32,
    pub offset: u32,
    pub pitch: u32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
}

pub const fn fourcc(code: &[u8; 4]) -> u32 {
    (code[0] as u32) | (code[1] as u32) << 8 | (code[2] as u32) << 16 | (code[3] as u32) << 24
}
pub const DRM_FORMAT_R8: u32 = fourcc(b"R8  ");
pub const DRM_FORMAT_GR88: u32 = fourcc(b"GR88");
const DRM_FORMAT_NV12: u32 = fourcc(b"NV12");

pub struct GpuFrame {
    frame: *mut ffi::AVFrame,
    pub planes: [Plane; 2],
    pub color: ColorInfo,
}

unsafe impl Send for GpuFrame {}

impl Drop for GpuFrame {
    fn drop(&mut self) {
        unsafe { ffi::av_frame_free(&mut self.frame) };
    }
}

impl GpuFrame {
    fn map(decoded: &ff::frame::Video, color: ColorInfo) -> Result<GpuFrame> {
        unsafe {
            let mut frame = ffi::av_frame_alloc();
            (*frame).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let r = ffi::av_hwframe_map(frame, decoded.as_ptr(), ffi::AV_HWFRAME_MAP_READ as i32);
            if r < 0 {
                ffi::av_frame_free(&mut frame);
                bail!("av_hwframe_map: {r}");
            }
            let mut gpu = GpuFrame {
                frame,
                planes: [Plane::EMPTY; 2],
                color,
            };
            let desc = &*((*frame).data[0] as *const ffi::AVDRMFrameDescriptor);
            let (w, h) = ((*frame).width as u32, (*frame).height as u32);
            let plane = |layer: usize, i: usize, fourcc: u32, width: u32, height: u32| {
                let p = desc.layers[layer].planes[i];
                let object = desc.objects[p.object_index as usize];
                Plane {
                    fourcc,
                    fd: object.fd,
                    offset: p.offset as u32,
                    pitch: p.pitch as u32,
                    modifier: object.format_modifier,
                    width,
                    height,
                }
            };
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            gpu.planes = match desc.nb_layers {
                1 if desc.layers[0].format == DRM_FORMAT_NV12 && desc.layers[0].nb_planes == 2 => [
                    plane(0, 0, DRM_FORMAT_R8, w, h),
                    plane(0, 1, DRM_FORMAT_GR88, cw, ch),
                ],
                2 if desc.layers[0].format == DRM_FORMAT_R8
                    && desc.layers[1].format == DRM_FORMAT_GR88 =>
                {
                    [
                        plane(0, 0, DRM_FORMAT_R8, w, h),
                        plane(1, 0, DRM_FORMAT_GR88, cw, ch),
                    ]
                }
                n => bail!(
                    "unexpected DRM layout: {n} layers, first format {:#x}",
                    desc.layers[0].format
                ),
            };
            Ok(gpu)
        }
    }
}

impl Plane {
    const EMPTY: Plane = Plane {
        fourcc: 0,
        fd: -1,
        offset: 0,
        pitch: 0,
        modifier: 0,
        width: 0,
        height: 0,
    };
}

fn color_info(frame: &ff::frame::Video) -> ColorInfo {
    use ff::util::color::{Range, Space};
    ColorInfo {
        bt709: frame.color_space() == Space::BT709,
        full_range: frame.color_range() == Range::JPEG,
    }
}

pub fn spawn(
    window_id: u32,
    frames: mpsc::Receiver<Received>,
    proxy: EventLoopProxy<UserEvent>,
    latest: Latest,
) {
    thread::Builder::new()
        .name(format!("decode-{window_id}"))
        .spawn(move || {
            if let Err(err) = run(frames, &proxy, &latest) {
                error!(window_id, "decoder stopped: {err:#}");
            }
        })
        .expect("spawn decode thread");
}

unsafe extern "C" fn pick_vaapi(
    _ctx: *mut ffi::AVCodecContext,
    mut formats: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    unsafe {
        let first = *formats;
        while *formats != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *formats == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            formats = formats.add(1);
        }
        first
    }
}

fn run(
    frames: mpsc::Receiver<Received>,
    proxy: &EventLoopProxy<UserEvent>,
    latest: &Latest,
) -> Result<()> {
    ff::init()?;
    let h264 = ff::decoder::find(codec::Id::H264).context("no H.264 decoder")?;
    let mut ctx = codec::context::Context::new_with_codec(h264);
    let hw = unsafe {
        let raw = ctx.as_mut_ptr();
        let mut device: *mut ffi::AVBufferRef = ptr::null_mut();
        let r = ffi::av_hwdevice_ctx_create(
            &mut device,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            ptr::null(),
            ptr::null_mut(),
            0,
        );
        if r >= 0 {
            (*raw).hw_device_ctx = ffi::av_buffer_ref(device);
            (*raw).get_format = Some(pick_vaapi);
            ffi::av_buffer_unref(&mut device);
        }
        (*raw).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
        (*raw).extra_hw_frames = 4;
        (*raw).thread_count = 1;
        r >= 0
    };
    if hw {
        debug!("decoding with VA-API");
    } else {
        warn!("VA-API unavailable, decoding in software");
    }
    let mut decoder = ctx.decoder().video()?;

    let mut decoded = ff::frame::Video::empty();
    let mut nv12 = ff::frame::Video::empty();
    let mut rgb = ff::frame::Video::empty();
    let mut scaler: Option<(scaling::Context, Pixel, u32, u32)> = None;

    let mut skipped = 0u32;
    let mut map_failed_logged = false;
    while let Ok(first) = frames.recv() {
        let batch: Vec<Received> = std::iter::once(first).chain(frames.try_iter()).collect();
        let newest = batch.len() - 1;
        for (i, recv) in batch.into_iter().enumerate() {
            let packet = ff::Packet::copy(&recv.data);
            if let Err(err) = decoder.send_packet(&packet) {
                warn!(frame = recv.header.frame_id, "decode error: {err}");
                continue;
            }
            while decoder.receive_frame(&mut decoded).is_ok() {
                if i != newest {
                    skipped += 1;
                    continue;
                }
                let w = (recv.header.visible_width as u32).min(decoded.width());
                let h = (recv.header.visible_height as u32).min(decoded.height());

                let mut picture = None;
                if decoded.format() == Pixel::VAAPI && GPU_PATH.load(Ordering::Relaxed) {
                    match GpuFrame::map(&decoded, color_info(&decoded)) {
                        Ok(gpu) => picture = Some(Picture::Gpu(gpu)),
                        Err(err) if !map_failed_logged => {
                            warn!("can't map the frame for the GPU path, copying instead: {err:#}");
                            map_failed_logged = true;
                        }
                        Err(_) => {}
                    }
                }
                if let Some(picture) = picture {
                    if !hand_over(latest, proxy, &recv, picture, w, h, &mut skipped) {
                        return Ok(());
                    }
                    continue;
                }

                let src = if decoded.format() == Pixel::VAAPI {
                    unsafe {
                        ffi::av_frame_unref(nv12.as_mut_ptr());
                        let r =
                            ffi::av_hwframe_transfer_data(nv12.as_mut_ptr(), decoded.as_ptr(), 0);
                        if r < 0 {
                            bail!("av_hwframe_transfer_data: {r}");
                        }
                    }
                    &nv12
                } else {
                    &decoded
                };
                let (sw, sh, fmt) = (src.width(), src.height(), src.format());
                if scaler
                    .as_ref()
                    .is_none_or(|s| (s.1, s.2, s.3) != (fmt, sw, sh))
                {
                    let ctx = scaling::Context::get(
                        fmt,
                        sw,
                        sh,
                        Pixel::BGRZ,
                        sw,
                        sh,
                        Flags::FAST_BILINEAR,
                    )
                    .map_err(|e| anyhow!("scaler: {e}"))?;
                    scaler = Some((ctx, fmt, sw, sh));
                    rgb = ff::frame::Video::empty();
                }
                scaler.as_mut().unwrap().0.run(src, &mut rgb)?;

                let stride = rgb.stride(0);
                let bytes = rgb.data(0);
                let (wu, hu) = (w as usize, h as usize);
                let mut pixels = vec![0u32; wu * hu];
                let out: &mut [u8] = bytemuck::cast_slice_mut(&mut pixels);
                for row in 0..hu {
                    out[row * wu * 4..(row + 1) * wu * 4]
                        .copy_from_slice(&bytes[row * stride..row * stride + wu * 4]);
                }
                if !hand_over(
                    latest,
                    proxy,
                    &recv,
                    Picture::Cpu(pixels),
                    w,
                    h,
                    &mut skipped,
                ) {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn hand_over(
    latest: &Latest,
    proxy: &EventLoopProxy<UserEvent>,
    recv: &Received,
    picture: Picture,
    width: u32,
    height: u32,
    skipped: &mut u32,
) -> bool {
    let frame = Decoded {
        width,
        height,
        picture,
        header: recv.header,
        received_ns: recv.received_ns,
        decoded_ns: clock::now_ns(),
        skipped: std::mem::take(skipped),
    };
    let window_id = recv.header.window_id;
    let replaced = latest.lock().unwrap().insert(window_id, frame);
    match replaced {
        Some(old) => {
            let mut slot = latest.lock().unwrap();
            if let Some(new) = slot.get_mut(&window_id) {
                new.skipped += old.skipped + 1;
            }
            true
        }
        None => proxy.send_event(UserEvent::FrameReady(window_id)).is_ok(),
    }
}
