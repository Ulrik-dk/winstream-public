use std::{
    ffi::OsString,
    fs::OpenOptions,
    os::fd::OwnedFd,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use proto::{Control, PointerGrab};
use smithay::{
    backend::{
        allocator::{Buffer as _, Fourcc, dmabuf::Dmabuf, gbm::GbmDevice},
        drm::DrmNode,
        egl::{EGLContext, EGLDisplay},
        input::{Axis, AxisSource, ButtonState, InputTime, KeyState},
        renderer::{
            Bind, ExportMem, ImportDma, Offscreen,
            damage::OutputDamageTracker,
            element::{AsRenderElements, surface::WaylandSurfaceRenderElement},
            gles::{GlesRenderer, GlesTexture},
            utils::on_commit_buffer_handler,
        },
    },
    desktop::{
        PopupKind, PopupManager, Window, WindowSurfaceType, find_popup_root_surface,
        get_popup_toplevel_coords, space::SpaceElement,
    },
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DndGrabHandler, GrabType, Source},
        keyboard::{FilterResult, Keycode, XkbConfig},
        pointer::{
            AxisFrame, ButtonEvent, CursorImageStatus, MotionEvent, PointerHandle,
            RelativeMotionEvent,
        },
    },
    output::{Mode as OutputMode, Output, PhysicalProperties, Subpixel},
    reexports::{
        calloop::{
            EventLoop, Interest, LoopHandle, LoopSignal, Mode as LoopMode, PostAction,
            channel::{Event as ChannelEvent, channel},
            generic::Generic,
            timer::{TimeoutAction, Timer},
        },
        wayland_protocols::xdg::shell::server::xdg_toplevel,
        wayland_server::{
            Client, Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::{wl_buffer, wl_output, wl_seat, wl_surface::WlSurface},
        },
    },
    utils::{
        DeviceFd, Logical, Physical, Point, Rectangle, SERIAL_COUNTER, Scale, Serial, Size,
        Transform,
    },
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface, with_states,
        },
        dmabuf::{DmabufFeedbackBuilder, DmabufGlobal, DmabufHandler, DmabufState, ImportNotifier},
        output::{OutputHandler, OutputManagerState},
        pointer_constraints::{
            ConstraintRemove, PointerConstraint, PointerConstraintsHandler,
            PointerConstraintsState, with_pointer_constraint,
        },
        relative_pointer::RelativePointerManagerState,
        seat::WaylandFocus,
        selection::{
            SelectionHandler, SelectionSource, SelectionTarget,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
        shell::xdg::{
            PopupSurface, PositionerState, ToplevelSurface, XdgShellHandler, XdgShellState,
            XdgToplevelSurfaceData,
        },
        shm::{ShmHandler, ShmState},
        socket::ListeningSocketSource,
        xwayland_shell::XWaylandShellState,
    },
    xwayland::{X11Wm, XWaylandClientData},
};
use tokio::sync::mpsc::UnboundedSender;
use tracing::{debug, info, warn};

pub mod apps;
mod selection;
mod xwm;
use xwm::{KbFocus, Overlay};

use crate::{
    Config, audio, clock,
    encoder::{self, EncodedFrame, EncoderCmd, EncoderHandle, Picture, RawFrame},
    gpu_copy,
    net::{self, NetEvent},
};

const RENDER_NODE: &str = "/dev/dri/renderD128";
const MIN_PICTURE: (i32, i32) = (160, 64);
const MAX_PICTURE: i32 = 4096;
const CUDA_RETRY: Duration = Duration::from_secs(2);

fn picture_side(need: i32, current: Option<i32>, screen: i32) -> i32 {
    let even = |v: i32| (v + 1) & !1;
    let need = even(need);
    if let Some(cur) = current
        && need <= cur
        && need * 2 >= cur
    {
        return cur;
    }
    let roomy = (need + need / 4 + 63) / 64 * 64;
    even(roomy.min(screen.max(need)).min(MAX_PICTURE.max(need)))
}

struct StreamWindow {
    id: u32,
    window: Window,
    title: String,
    app_id: String,
    target: Option<(GlesTexture, (i32, i32))>,
    damage: Option<OutputDamageTracker>,
    encoder: Option<EncoderHandle>,
    pending_commit: Option<Instant>,
    next_frame_id: u64,
    copy: Option<gpu_copy::WindowCopy>,
}

impl StreamWindow {
    fn xdg(&self) -> Option<&ToplevelSurface> {
        self.window.toplevel()
    }

    fn wl_surface(&self) -> Option<WlSurface> {
        self.window.wl_surface().map(|s| s.into_owned())
    }
}

type RenderedWindow = ((i32, i32), (i32, i32), Option<Vec<u8>>);

pub struct State {
    display_handle: DisplayHandle,
    loop_handle: LoopHandle<'static, State>,
    loop_signal: LoopSignal,
    socket_name: OsString,

    compositor_state: CompositorState,
    xdg_shell_state: XdgShellState,
    shm_state: ShmState,
    data_device_state: DataDeviceState,
    primary_selection_state: PrimarySelectionState,
    selections: selection::Selections,
    dmabuf_state: DmabufState,
    _dmabuf_global: DmabufGlobal,
    _output_manager_state: OutputManagerState,
    seat_state: SeatState<State>,
    seat: Seat<State>,
    _relative_pointer_state: RelativePointerManagerState,
    _pointer_constraints_state: PointerConstraintsState,
    xwayland_shell_state: XWaylandShellState,
    xwm: Option<X11Wm>,
    x_display: Option<u32>,

    windows: Vec<StreamWindow>,
    overlays: Vec<Overlay>,
    next_window_id: u32,
    popups: PopupManager,
    output: Output,
    pointer_window: Option<u32>,
    focused_window: Option<u32>,

    renderer: GlesRenderer,

    frames_tx: UnboundedSender<EncodedFrame>,
    settings: encoder::Settings,
    to_client: UnboundedSender<Control>,
    streaming: bool,
    render_scheduled: bool,
    stats: Stats,

    apps: Vec<apps::App>,
    queued_apps: Vec<Vec<String>>,
    apps_ready: bool,
    end_with_apps: bool,
    pulse_sink: Option<String>,
    launch_socket: Option<std::path::PathBuf>,
    result: Option<Result<()>>,
    last_render: RenderTiming,
    cuda_copy: Option<gpu_copy::CudaCopy>,
    cuda_retry_at: Option<Instant>,
    busy_skips: u64,
}

#[derive(Default)]
struct Stats {
    since: Option<Instant>,
    frames: u32,
    commit_to_render_us: u64,
    readback_us: u64,
    max_readback_us: u64,
    parts: RenderTiming,
}

#[derive(Default, Clone, Copy)]
struct RenderTiming {
    render_us: u64,
    copy_us: u64,
    map_us: u64,
    to_vec_us: u64,
}

#[derive(Default)]
struct ClientState {
    compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}

pub fn run(config: Config) -> Result<()> {
    let Config {
        listen,
        authority,
        width,
        height,
        settings,
        command,
    } = config;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(RENDER_NODE)
        .with_context(|| format!("opening {RENDER_NODE}"))?;
    let gbm = GbmDevice::new(DeviceFd::from(OwnedFd::from(file))).context("creating GBM device")?;
    let egl = unsafe { EGLDisplay::new(gbm) }.context("creating EGL display on GBM")?;
    let context = EGLContext::new(&egl).context("creating EGL context")?;
    let renderer = unsafe { GlesRenderer::new(context) }.context("creating GLES renderer")?;
    let node = DrmNode::from_path(RENDER_NODE).context("render node")?;
    info!(?node, "GPU ready (headless)");

    let mut event_loop: EventLoop<'static, State> = EventLoop::try_new()?;
    let display: Display<State> = Display::new()?;
    let dh = display.handle();

    let compositor_state = CompositorState::new::<State>(&dh);
    let xdg_shell_state = XdgShellState::new::<State>(&dh);
    let shm_state = ShmState::new::<State>(&dh, vec![]);
    let output_manager_state = OutputManagerState::new_with_xdg_output::<State>(&dh);
    let data_device_state = DataDeviceState::new::<State>(&dh);
    let primary_selection_state = PrimarySelectionState::new::<State>(&dh);

    let feedback = DmabufFeedbackBuilder::new(node.dev_id(), renderer.dmabuf_formats())
        .build()
        .context("dmabuf feedback")?;
    let mut dmabuf_state = DmabufState::new();
    let dmabuf_global = dmabuf_state.create_global_with_default_feedback::<State>(&dh, &feedback);

    let relative_pointer_state = RelativePointerManagerState::new::<State>(&dh);
    let pointer_constraints_state = PointerConstraintsState::new::<State>(&dh);
    let xwayland_shell_state = XWaylandShellState::new::<State>(&dh);

    let mut seat_state = SeatState::new();
    let mut seat = seat_state.new_wl_seat(&dh, "remote");
    seat.add_keyboard(Default::default(), 200, 25)
        .map_err(|e| anyhow!("keyboard: {e:?}"))?;
    seat.add_pointer();

    let output = Output::new(
        "headless".into(),
        PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "winstream".into(),
            model: "headless".into(),
            serial_number: "0".into(),
        },
    );
    let _output_global = output.create_global::<State>(&dh);
    let output_mode = OutputMode {
        size: (width, height).into(),
        refresh: settings.fps as i32 * 1000,
    };
    output.change_current_state(
        Some(output_mode),
        Some(Transform::Normal),
        None,
        Some((0, 0).into()),
    );
    output.set_preferred(output_mode);

    let listening = ListeningSocketSource::new_auto()?;
    let socket_name = listening.socket_name().to_os_string();
    let handle = event_loop.handle();
    handle
        .insert_source(listening, |stream, _, state: &mut State| {
            if let Err(err) = state
                .display_handle
                .insert_client(stream, Arc::new(ClientState::default()))
            {
                warn!(?err, "failed to add client");
            }
        })
        .map_err(|e| anyhow!("socket source: {e}"))?;
    handle
        .insert_source(
            Generic::new(display, Interest::READ, LoopMode::Level),
            |_, display, state: &mut State| {
                unsafe { display.get_mut().dispatch_clients(state).unwrap() };
                Ok(PostAction::Continue)
            },
        )
        .map_err(|e| anyhow!("display source: {e}"))?;

    handle
        .insert_source(
            Timer::from_duration(settings.frame_interval()),
            move |_, _, state: &mut State| {
                if crate::STOP.load(std::sync::atomic::Ordering::Relaxed) {
                    state.finish(Ok(()));
                }
                state.send_frame_callbacks();
                TimeoutAction::ToDuration(state.settings.frame_interval())
            },
        )
        .map_err(|e| anyhow!("timer: {e}"))?;

    let mut audio_sink = None;
    let (ftx, frx) = tokio::sync::mpsc::unbounded_channel();
    let (ctx, crx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, events_rx) = channel::<NetEvent>();
    let (atx, arx) = tokio::sync::mpsc::channel(20);
    match audio::Sink::create() {
        Ok(sink) => {
            audio::spawn_capture(&sink.name, atx);
            audio_sink = Some(sink);
        }
        Err(err) => warn!("no audio: {err:#}"),
    }
    net::spawn(listen, authority, frx, crx, ctx.clone(), arx, events_tx);
    handle
        .insert_source(events_rx, |event, _, state: &mut State| {
            if let ChannelEvent::Msg(event) = event {
                state.on_net_event(event);
            }
        })
        .map_err(|e| anyhow!("net events: {e}"))?;

    let (selection_tx, selection_rx) = channel::<(SelectionTarget, String)>();
    handle
        .insert_source(selection_rx, |event, _, state: &mut State| {
            if let ChannelEvent::Msg((target, text)) = event {
                state.selection_read(target, text);
            }
        })
        .map_err(|e| anyhow!("selection channel: {e}"))?;

    let cuda_copy = if std::env::var("WINSTREAM_SERVER_CPU").is_ok_and(|v| v == "1") {
        info!("reading pictures back to memory (CPU path)");
        None
    } else {
        match gpu_copy::CudaCopy::new() {
            Ok(c) => {
                info!("pictures go to the encoder GPU to GPU (CUDA)");
                Some(c)
            }
            Err(err) => {
                warn!("no CUDA copy, reading pictures back instead: {err:#}");
                None
            }
        }
    };

    let mut state = State {
        display_handle: dh,
        loop_handle: handle,
        loop_signal: event_loop.get_signal(),
        socket_name,
        compositor_state,
        xdg_shell_state,
        shm_state,
        data_device_state,
        primary_selection_state,
        selections: selection::Selections::new(selection_tx),
        dmabuf_state,
        _dmabuf_global: dmabuf_global,
        _output_manager_state: output_manager_state,
        seat_state,
        seat,
        _relative_pointer_state: relative_pointer_state,
        _pointer_constraints_state: pointer_constraints_state,
        xwayland_shell_state,
        xwm: None,
        x_display: None,
        windows: Vec::new(),
        overlays: Vec::new(),
        next_window_id: 1,
        popups: PopupManager::default(),
        output,
        pointer_window: None,
        focused_window: None,
        renderer,
        frames_tx: ftx,
        settings,
        to_client: ctx,
        streaming: false,
        render_scheduled: false,
        stats: Stats::default(),
        apps: Vec::new(),
        end_with_apps: !command.is_empty(),
        queued_apps: if command.is_empty() {
            Vec::new()
        } else {
            vec![command]
        },
        apps_ready: false,
        pulse_sink: audio_sink.as_ref().map(|s: &audio::Sink| s.name.clone()),
        launch_socket: None,
        last_render: RenderTiming::default(),
        cuda_copy,
        cuda_retry_at: None,
        busy_skips: 0,
        result: None,
    };

    state.listen_for_launches()?;
    if !state.start_xwayland() {
        state.apps_ready();
    }

    event_loop.run(None, &mut state, |state| state.reap_apps())?;

    apps::stop_apps(&mut state.apps);
    if let Some(path) = state.launch_socket.take() {
        let _ = std::fs::remove_file(path);
    }
    net::remove_auth_file();
    for window in state.windows.drain(..) {
        if let Some(encoder) = window.encoder {
            encoder.shutdown();
        }
    }
    drop(audio_sink);
    state.result.take().unwrap_or(Ok(()))
}

impl State {
    fn mark_dirty(&mut self, id: u32) {
        if let Some(idx) = self.window_index(id) {
            self.windows[idx]
                .pending_commit
                .get_or_insert_with(Instant::now);
            self.schedule_render();
        }
    }

    fn remove_window(&mut self, idx: usize) {
        let w = self.windows.remove(idx);
        info!(window = w.id, title = %w.title, "window closed");
        self.send_to_client(Control::WindowClosed { window: w.id });
        if self.pointer_window == Some(w.id) {
            self.pointer_window = None;
        }
        if self.focused_window == Some(w.id) {
            self.focused_window = None;
        }
        self.overlays.retain(|o| o.owner != w.id);
        if let Some(encoder) = w.encoder {
            encoder.shutdown();
        }
    }

    fn xdg_window_index(&self, surface: &ToplevelSurface) -> Option<usize> {
        self.windows.iter().position(|w| {
            w.xdg()
                .is_some_and(|t| t.wl_surface() == surface.wl_surface())
        })
    }

    fn window_index(&self, id: u32) -> Option<usize> {
        self.windows.iter().position(|w| w.id == id)
    }

    fn window_for_surface(&self, surface: &WlSurface) -> Option<usize> {
        let mut root = surface.clone();
        while let Some(parent) = get_parent(&root) {
            root = parent;
        }
        if let Some(popup) = self.popups.find_popup(&root)
            && let Ok(toplevel) = find_popup_root_surface(&popup)
        {
            root = toplevel;
        }
        if let Some(idx) = self
            .windows
            .iter()
            .position(|w| w.wl_surface().as_ref() == Some(&root))
        {
            return Some(idx);
        }
        let owner = self
            .overlays
            .iter()
            .find(|o| o.window.wl_surface().as_deref() == Some(&root))?
            .owner;
        self.window_index(owner)
    }

    fn send_to_client(&self, msg: Control) {
        if self.streaming {
            let _ = self.to_client.send(msg);
        }
    }

    fn announce(&self, w: &StreamWindow) {
        let geo = w.window.geometry();
        let parent = if let Some(toplevel) = w.xdg() {
            toplevel.parent().and_then(|p| {
                self.windows
                    .iter()
                    .find(|o| o.wl_surface().as_ref() == Some(&p))
            })
        } else {
            w.window
                .x11_surface()
                .and_then(|x| x.is_transient_for())
                .and_then(|p| {
                    self.windows
                        .iter()
                        .find(|o| o.window.x11_surface().is_some_and(|s| s.window_id() == p))
                })
        }
        .map_or(0, |o| o.id);
        self.send_to_client(Control::WindowOpened {
            window: w.id,
            parent,
            width: geo.size.w.max(0) as u32,
            height: geo.size.h.max(0) as u32,
            title: w.title.clone(),
            app_id: w.app_id.clone(),
        });
    }

    fn render_window(&mut self, idx: usize, read_back: bool) -> Result<RenderedWindow> {
        let renderer = &mut self.renderer;
        let sw = &mut self.windows[idx];
        let geo = sw.window.geometry();
        let visible = (geo.size.w.max(1), geo.size.h.max(1));
        let screen = self
            .output
            .current_mode()
            .map_or((MAX_PICTURE, MAX_PICTURE), |m| (m.size.w, m.size.h));
        let current = sw.target.as_ref().map(|(_, s)| *s);
        let size = (
            picture_side(visible.0.max(MIN_PICTURE.0), current.map(|s| s.0), screen.0),
            picture_side(visible.1.max(MIN_PICTURE.1), current.map(|s| s.1), screen.1),
        );
        if current != Some(size) {
            sw.target = None;
            let texture: GlesTexture = renderer
                .create_buffer(Fourcc::Abgr8888, Size::from(size))
                .context("creating offscreen target")?;
            sw.target = Some((texture, size));
            sw.damage = Some(OutputDamageTracker::new(size, 1.0, Transform::Normal));
        }

        let origin: Point<i32, Physical> = Point::from((-geo.loc.x, -geo.loc.y));
        let mut elements: Vec<WaylandSurfaceRenderElement<GlesRenderer>> = Vec::new();
        for overlay in self.overlays.iter().rev().filter(|o| o.owner == sw.id) {
            let at = origin + overlay.position().to_physical(1);
            elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
                &overlay.window,
                renderer,
                at,
                Scale::from(1.0),
                1.0,
            ));
        }
        elements.extend(AsRenderElements::<GlesRenderer>::render_elements(
            &sw.window,
            renderer,
            origin,
            Scale::from(1.0),
            1.0,
        ));

        let t0 = Instant::now();
        let (texture, _) = sw.target.as_mut().unwrap();
        let mut framebuffer = renderer.bind(texture)?;
        sw.damage
            .as_mut()
            .unwrap()
            .render_output(
                renderer,
                &mut framebuffer,
                0,
                &elements,
                [0.0, 0.0, 0.0, 1.0],
            )
            .map_err(|e| anyhow!("render: {e:?}"))?;
        if !read_back {
            self.last_render = RenderTiming {
                render_us: (Instant::now() - t0).as_micros() as u64,
                ..Default::default()
            };
            return Ok((size, visible, None));
        }
        let t1 = Instant::now();
        let mapping = renderer.copy_framebuffer(
            &framebuffer,
            Rectangle::from_size(Size::from(size)),
            Fourcc::Abgr8888,
        )?;
        let t2 = Instant::now();
        let pixels = renderer.map_texture(&mapping)?;
        let t3 = Instant::now();
        let len = (size.0 * size.1 * 4) as usize;
        if pixels.len() < len {
            bail!("readback too short: {} bytes", pixels.len());
        }
        let out = pixels[..len].to_vec();
        let t4 = Instant::now();
        let us = |a: Instant, b: Instant| (b - a).as_micros() as u64;
        self.last_render = RenderTiming {
            render_us: us(t0, t1),
            copy_us: us(t1, t2),
            map_us: us(t2, t3),
            to_vec_us: us(t3, t4),
        };
        Ok((size, visible, Some(out)))
    }

    fn on_net_event(&mut self, event: NetEvent) {
        match event {
            NetEvent::Connected(addr) => {
                info!(%addr, windows = self.windows.len(), "streaming to client");
                self.streaming = true;
                self.stats = Stats::default();
                self.resend_selections();
                for w in &self.windows {
                    self.announce(w);
                }
                let now = Instant::now();
                for w in &mut self.windows {
                    if let Some(encoder) = &w.encoder {
                        let _ = encoder.tx.send(EncoderCmd::Restart);
                    }
                    w.pending_commit = Some(now);
                }
                self.schedule_render();
            }
            NetEvent::Disconnected => {
                info!("client disconnected; not streaming");
                self.streaming = false;
                self.release_all_input();
            }
            NetEvent::Input(msg) => self.on_input(msg),
            NetEvent::Stopped => {
                self.finish(Err(anyhow!(
                    "network thread stopped; no client can connect"
                )));
            }
        }
    }

    fn schedule_render(&mut self) {
        if !self.streaming || self.render_scheduled {
            return;
        }
        self.render_scheduled = true;
        self.loop_handle.insert_idle(|state| {
            state.render_scheduled = false;
            if let Err(err) = state.render_and_encode() {
                state.finish(Err(err));
            }
        });
    }

    fn render_and_encode(&mut self) -> Result<()> {
        for idx in 0..self.windows.len() {
            let Some(commit) = self.windows[idx].pending_commit.take() else {
                continue;
            };
            if self.windows[idx].window.geometry().size.w <= 0 {
                continue;
            }
            let render_start = Instant::now();
            let use_cuda =
                self.cuda_copy.is_some() && self.cuda_retry_at.is_none_or(|t| Instant::now() >= t);
            let (size, visible, pixels) = self.render_window(idx, !use_cuda)?;
            let frame_id = self.windows[idx].next_frame_id;
            self.windows[idx].next_frame_id += 1;
            let picture = match pixels {
                Some(pixels) => Picture::Cpu(pixels),
                None => {
                    let t = Instant::now();
                    let cuda = self.cuda_copy.as_ref().expect("GPU path");
                    let sw = &mut self.windows[idx];
                    let tex_id = sw.target.as_ref().expect("rendered").0.tex_id();
                    match cuda.copy(&mut sw.copy, &mut self.renderer, tex_id, size) {
                        Ok(Some(slot)) => {
                            if self.cuda_retry_at.take().is_some() {
                                info!("CUDA copy works again");
                            }
                            self.last_render.copy_us = (Instant::now() - t).as_micros() as u64;
                            Picture::Gpu(slot)
                        }
                        Ok(None) => {
                            self.busy_skips += 1;
                            continue;
                        }
                        Err(err) => {
                            warn!(
                                "CUDA copy failed, reading pictures back for {CUDA_RETRY:?}: {err:#}"
                            );
                            for w in &mut self.windows {
                                w.copy = None;
                            }
                            self.cuda_retry_at = Some(Instant::now() + CUDA_RETRY);
                            continue;
                        }
                    }
                }
            };
            let done = Instant::now();

            let frames_tx = self.frames_tx.clone();
            let settings = self.settings;
            let sw = &mut self.windows[idx];
            let id = sw.id;
            let encoder = sw
                .encoder
                .get_or_insert_with(|| encoder::spawn(id, frames_tx, settings));
            if encoder
                .tx
                .send(EncoderCmd::Frame(RawFrame {
                    picture,
                    width: size.0 as u32,
                    height: size.1 as u32,
                    visible_width: visible.0 as u32,
                    visible_height: visible.1 as u32,
                    frame_id,
                    commit_ns: clock::ns_at(commit),
                    sent_ns: clock::now_ns(),
                }))
                .is_err()
            {
                warn!(window = id, "encoder thread gone; starting a new one");
                if let Some(gone) = sw.encoder.take() {
                    gone.shutdown();
                }
            }

            let s = &mut self.stats;
            let since = *s.since.get_or_insert(done);
            s.frames += 1;
            s.commit_to_render_us += (render_start - commit).as_micros() as u64;
            let readback = (done - render_start).as_micros() as u64;
            s.readback_us += readback;
            s.max_readback_us = s.max_readback_us.max(readback);
            let r = self.last_render;
            s.parts.render_us += r.render_us;
            s.parts.copy_us += r.copy_us;
            s.parts.map_us += r.map_us;
            s.parts.to_vec_us += r.to_vec_us;
            if done - since >= Duration::from_secs(1) {
                let n = s.frames.max(1) as f64;
                let ms = |us: u64| format!("{:.2}", us as f64 / n / 1000.0);
                info!(
                    fps = s.frames,
                    windows = self.windows.len(),
                    commit_to_render_ms = ms(s.commit_to_render_us),
                    render_readback_ms = ms(s.readback_us),
                    max_ms = format!("{:.2}", s.max_readback_us as f64 / 1000.0),
                    render_ms = ms(s.parts.render_us),
                    copy_ms = ms(s.parts.copy_us),
                    map_ms = ms(s.parts.map_us),
                    to_vec_ms = ms(s.parts.to_vec_us),
                    busy_skips = self.busy_skips,
                    gpu = self.cuda_copy.is_some() && self.cuda_retry_at.is_none(),
                    "server"
                );
                self.busy_skips = 0;
                self.stats = Stats::default();
            }
        }
        Ok(())
    }

    fn on_input(&mut self, msg: Control) {
        let serial = SERIAL_COUNTER.next_serial();
        let time = InputTime::now();
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        let pointer = self.seat.get_pointer().expect("pointer");
        match msg {
            Control::Keymap {
                rules,
                model,
                layout,
                variant,
                options,
            } => {
                let config = XkbConfig {
                    rules: &rules,
                    model: &model,
                    layout: &layout,
                    variant: &variant,
                    options: (!options.is_empty()).then_some(options.clone()),
                };
                match keyboard.set_xkb_config(self, config) {
                    Ok(()) => {
                        info!(%layout, %variant, %model, %options, "keyboard layout from client")
                    }
                    Err(err) => warn!(?err, %layout, "client keyboard layout rejected"),
                }
            }
            Control::Key { code, pressed } => {
                let state = if pressed {
                    KeyState::Pressed
                } else {
                    KeyState::Released
                };
                keyboard.input::<(), _>(
                    self,
                    Keycode::new(code + 8),
                    state,
                    serial,
                    time,
                    |_, _, _| FilterResult::Forward,
                );
            }
            Control::PointerMotion { window, x, y } => {
                self.pointer_window = Some(window);
                let location = Point::from((x, y));
                let under = self.surface_under(window, location);
                if let Some((surface, _)) = &under
                    && self.pointer_locked(surface, &pointer)
                {
                    return;
                }
                pointer.motion(
                    self,
                    under.clone(),
                    &MotionEvent {
                        location,
                        serial,
                        time,
                    },
                );
                pointer.frame(self);
                if let Some((surface, _)) = under {
                    self.activate_constraint(&surface, &pointer);
                }
            }
            Control::RelativeMotion { dx, dy } => {
                let delta = Point::from((dx, dy));
                let focus = self
                    .pointer_window
                    .and_then(|w| self.surface_under(w, pointer.current_location()));
                pointer.relative_motion(
                    self,
                    focus,
                    &RelativeMotionEvent {
                        delta,
                        delta_unaccel: delta,
                        time,
                    },
                );
                pointer.frame(self);
            }
            Control::Display {
                width,
                height,
                refresh_mhz,
            } => {
                let mode = OutputMode {
                    size: (width as i32, height as i32).into(),
                    refresh: self.settings.fps as i32 * 1000,
                };
                self.output
                    .change_current_state(Some(mode), None, None, None);
                self.output.set_preferred(mode);
                info!(width, height, refresh_mhz, "client display");
            }
            Control::PointerButton { button, pressed } => {
                if pressed && let Some(window) = self.pointer_window {
                    self.focus_window(window, serial, false);
                }
                let state = if pressed {
                    ButtonState::Pressed
                } else {
                    ButtonState::Released
                };
                pointer.button(
                    self,
                    &ButtonEvent {
                        serial,
                        time,
                        button,
                        state,
                    },
                );
                pointer.frame(self);
                if !pressed && button == 0x110 {
                    self.reread_primary_soon();
                }
            }
            Control::Scroll {
                dx,
                dy,
                steps_x,
                steps_y,
            } => {
                let source = if steps_x != 0 || steps_y != 0 {
                    AxisSource::Wheel
                } else {
                    AxisSource::Finger
                };
                let mut frame = AxisFrame::new(time).source(source);
                if dx != 0.0 {
                    frame = frame.value(Axis::Horizontal, dx);
                    if steps_x != 0 {
                        frame = frame.v120(Axis::Horizontal, steps_x);
                    }
                }
                if dy != 0.0 {
                    frame = frame.value(Axis::Vertical, dy);
                    if steps_y != 0 {
                        frame = frame.v120(Axis::Vertical, steps_y);
                    }
                }
                pointer.axis(self, frame);
                pointer.frame(self);
            }
            Control::Focus { window } => self.focus_window(window, serial, true),
            Control::Resize {
                window,
                width,
                height,
            } => {
                if let Some(idx) = self.window_index(window) {
                    let w = &self.windows[idx];
                    if let Some(toplevel) = w.xdg() {
                        toplevel.with_pending_state(|state| {
                            state.size = Some((width as i32, height as i32).into());
                        });
                        toplevel.send_pending_configure();
                    } else if let Some(x) = w.window.x11_surface() {
                        let _ = x.configure(Rectangle::new(
                            (0, 0).into(),
                            (width as i32, height as i32).into(),
                        ));
                    }
                    debug!(window, width, height, "resize requested");
                }
            }
            Control::Close { window } => {
                if let Some(idx) = self.window_index(window) {
                    info!(window, "close requested by client");
                    let w = &self.windows[idx];
                    if let Some(toplevel) = w.xdg() {
                        toplevel.send_close();
                    } else if let Some(x) = w.window.x11_surface() {
                        let _ = x.close();
                    }
                }
            }
            Control::Selection { primary, text } => self.selection_from_laptop(primary, text),
            Control::Feedback {
                window,
                received,
                shown,
                last_frame_id,
                latency_us,
                ..
            } => {
                if let Some(idx) = self.window_index(window)
                    && let Some(encoder) = &self.windows[idx].encoder
                {
                    let _ = encoder.tx.send(EncoderCmd::Feedback {
                        received,
                        shown,
                        last_frame_id,
                        latency: Duration::from_micros(latency_us as u64),
                    });
                }
            }
            _ => {}
        }
    }

    fn focus_window(&mut self, window: u32, serial: Serial, reactivate: bool) {
        let again = self.focused_window == Some(window);
        if !again {
            info!(window, from = ?self.focused_window, "focusing window");
        } else if reactivate {
            info!(window, "reactivating window");
        }
        let Some(idx) = self.window_index(window) else {
            return;
        };
        for (i, w) in self.windows.iter().enumerate() {
            if let Some(toplevel) = w.xdg() {
                toplevel.with_pending_state(|state| {
                    if i == idx {
                        state.states.set(xdg_toplevel::State::Activated);
                    } else {
                        state.states.unset(xdg_toplevel::State::Activated);
                    }
                });
                toplevel.send_pending_configure();
            } else if let Some(x) = w.window.x11_surface() {
                if i == idx && x.is_hidden() {
                    info!(window, "restoring minimized window");
                    let _ = x.set_hidden(false);
                }
                let _ = x.set_activated(i == idx);
            }
        }
        let target = if let Some(toplevel) = self.windows[idx].xdg() {
            KbFocus::Wayland(toplevel.wl_surface().clone())
        } else {
            let x = self.windows[idx]
                .window
                .x11_surface()
                .expect("x11 window")
                .clone();
            if let Some(xwm) = &mut self.xwm {
                let _ = xwm.raise_window(&x);
            }
            KbFocus::X11(x)
        };
        self.focused_window = Some(window);
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        if again && reactivate {
            keyboard.set_focus(self, None, serial);
        }
        keyboard.set_focus(self, Some(target), SERIAL_COUNTER.next_serial());
    }

    fn pointer_locked(&self, surface: &WlSurface, pointer: &PointerHandle<State>) -> bool {
        with_pointer_constraint(surface, pointer, |c| {
            c.is_some_and(|c| c.is_active() && matches!(&*c, PointerConstraint::Locked(_)))
        })
    }

    fn activate_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<State>) {
        let mode = with_pointer_constraint(surface, pointer, |c| {
            let c = c?;
            if c.is_active() {
                return None;
            }
            c.activate();
            Some(match &*c {
                PointerConstraint::Locked(_) => PointerGrab::Locked,
                PointerConstraint::Confined(_) => PointerGrab::Confined,
            })
        });
        if let Some(mode) = mode
            && let Some(idx) = self.window_for_surface(surface)
        {
            let window = self.windows[idx].id;
            info!(window, ?mode, "app grabbed the pointer");
            self.send_to_client(Control::PointerGrab { window, mode });
        }
    }

    fn set_fullscreen(&mut self, idx: usize, on: bool) {
        let w = &self.windows[idx];
        let window = w.id;
        if let Some(toplevel) = w.xdg() {
            toplevel.with_pending_state(|state| {
                if on {
                    state.states.set(xdg_toplevel::State::Fullscreen);
                } else {
                    state.states.unset(xdg_toplevel::State::Fullscreen);
                }
            });
            toplevel.send_pending_configure();
        } else if let Some(x) = w.window.x11_surface() {
            let _ = x.set_fullscreen(on);
        }
        info!(window, on, "app asked for fullscreen");
        self.send_to_client(Control::Fullscreen { window, on });
    }

    fn release_all_input(&mut self) {
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        for code in keyboard.pressed_keys() {
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.input::<(), _>(
                self,
                code,
                KeyState::Released,
                serial,
                InputTime::now(),
                |_, _, _| FilterResult::Forward,
            );
        }
    }

    fn surface_under(
        &self,
        window: u32,
        location: Point<f64, Logical>,
    ) -> Option<(WlSurface, Point<f64, Logical>)> {
        let w = &self.windows[self.window_index(window)?];
        let geo_loc = w.window.geometry().loc.to_f64();
        let root = location + geo_loc;
        for overlay in self.overlays.iter().rev().filter(|o| o.owner == window) {
            let pos = overlay.position().to_f64();
            if let Some((surface, surface_loc)) = overlay
                .window
                .surface_under(root - pos, WindowSurfaceType::ALL)
            {
                return Some((surface, surface_loc.to_f64() + pos - geo_loc));
            }
        }
        w.window
            .surface_under(location + geo_loc, WindowSurfaceType::ALL)
            .map(|(surface, surface_loc)| (surface, surface_loc.to_f64() - geo_loc))
    }

    fn send_frame_callbacks(&mut self) {
        let output = self.output.clone();
        let now = Duration::from_nanos(clock::now_ns());
        for w in &self.windows {
            w.window
                .send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
            w.window.refresh();
        }
        for o in &self.overlays {
            o.window
                .send_frame(&output, now, Some(Duration::ZERO), |_, _| {
                    Some(output.clone())
                });
        }
        self.popups.cleanup();
        let _ = self.display_handle.flush_clients();
    }

    fn finish(&mut self, result: Result<()>) {
        if self.result.is_none() {
            self.result = Some(result);
        }
        self.loop_signal.stop();
    }

    fn unconstrain_popup(&self, popup: &PopupSurface) {
        let Ok(root) = find_popup_root_surface(&PopupKind::Xdg(popup.clone())) else {
            return;
        };
        let Some(w) = self
            .windows
            .iter()
            .find(|w| w.wl_surface().as_ref() == Some(&root))
        else {
            return;
        };
        let geo = w.window.geometry();
        let mut target = Rectangle::from_size(geo.size);
        target.loc -= get_popup_toplevel_coords(&PopupKind::Xdg(popup.clone()));
        popup.with_pending_state(|state| {
            state.geometry = state.positioner.get_unconstrained_geometry(target);
        });
    }

    fn toplevel_strings(surface: &WlSurface) -> (String, String) {
        with_states(surface, |states| {
            let data = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .unwrap()
                .lock()
                .unwrap();
            (
                data.title.clone().unwrap_or_default(),
                data.app_id.clone().unwrap_or_default(),
            )
        })
    }
}

fn group_alive(group: i32) -> bool {
    unsafe {
        while libc::waitpid(-group, std::ptr::null_mut(), libc::WNOHANG) > 0 {}
        libc::kill(-group, 0) == 0
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(xwayland) = client.get_data::<XWaylandClientData>() {
            return &xwayland.compositor_state;
        }
        &client
            .get_data::<ClientState>()
            .expect("client data")
            .compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        let idx = self.window_for_surface(surface);
        if let Some(idx) = idx
            && !is_sync_subsurface(surface)
        {
            self.windows[idx].window.on_commit();
        }

        if let Some(toplevel) = self
            .windows
            .iter()
            .find_map(|w| w.xdg().filter(|t| t.wl_surface() == surface))
        {
            let configured = with_states(surface, |states| {
                states
                    .data_map
                    .get::<XdgToplevelSurfaceData>()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .initial_configure_sent
            });
            if !configured {
                toplevel.send_configure();
            }
        }

        self.popups.commit(surface);
        if let Some(PopupKind::Xdg(popup)) = self.popups.find_popup(surface)
            && !popup.is_initial_configure_sent()
        {
            let _ = popup.send_configure();
        }

        if let Some(idx) = idx {
            if self.windows[idx].pending_commit.is_none() {
                self.windows[idx].pending_commit = Some(Instant::now());
            }
            self.schedule_render();
        }
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.shm_state
    }
}

impl DmabufHandler for State {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_imported(
        &mut self,
        _global: &DmabufGlobal,
        dmabuf: Dmabuf,
        notifier: ImportNotifier,
    ) {
        if self.renderer.import_dmabuf(&dmabuf, None).is_ok() {
            debug!(format = ?dmabuf.format(), "dmabuf imported");
            let _ = notifier.successful::<State>();
        } else {
            warn!("dmabuf import failed");
            notifier.failed();
        }
    }
}

impl XdgShellHandler for State {
    fn xdg_shell_state(&mut self) -> &mut XdgShellState {
        &mut self.xdg_shell_state
    }

    fn new_toplevel(&mut self, surface: ToplevelSurface) {
        surface.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        let (title, app_id) = Self::toplevel_strings(surface.wl_surface());
        let id = self.next_window_id;
        self.next_window_id += 1;
        info!(window = id, %title, %app_id, "window opened");
        let wl_surface = surface.wl_surface().clone();
        let w = StreamWindow {
            id,
            window: Window::new_wayland_window(surface),
            title,
            app_id,
            target: None,
            damage: None,
            encoder: None,
            pending_commit: None,
            next_frame_id: 0,
            copy: None,
        };
        self.announce(&w);
        self.windows.push(w);
        self.focused_window = Some(id);
        let keyboard = self.seat.get_keyboard().expect("keyboard");
        keyboard.set_focus(
            self,
            Some(KbFocus::Wayland(wl_surface)),
            SERIAL_COUNTER.next_serial(),
        );
    }

    fn toplevel_destroyed(&mut self, surface: ToplevelSurface) {
        if let Some(idx) = self.xdg_window_index(&surface) {
            self.remove_window(idx);
        }
    }

    fn title_changed(&mut self, surface: ToplevelSurface) {
        let (title, app_id) = Self::toplevel_strings(surface.wl_surface());
        if let Some(idx) = self.xdg_window_index(&surface) {
            let w = &mut self.windows[idx];
            w.title = title.clone();
            w.app_id = app_id.clone();
            let window = w.id;
            self.send_to_client(Control::WindowTitle {
                window,
                title,
                app_id,
            });
        }
    }

    fn app_id_changed(&mut self, surface: ToplevelSurface) {
        self.title_changed(surface);
    }

    fn fullscreen_request(
        &mut self,
        surface: ToplevelSurface,
        _output: Option<wl_output::WlOutput>,
    ) {
        if let Some(idx) = self.xdg_window_index(&surface) {
            self.set_fullscreen(idx, true);
        }
    }

    fn unfullscreen_request(&mut self, surface: ToplevelSurface) {
        if let Some(idx) = self.xdg_window_index(&surface) {
            self.set_fullscreen(idx, false);
        }
    }

    fn new_popup(&mut self, surface: PopupSurface, _positioner: PositionerState) {
        self.unconstrain_popup(&surface);
        let _ = self.popups.track_popup(PopupKind::Xdg(surface));
    }

    fn reposition_request(
        &mut self,
        surface: PopupSurface,
        positioner: PositionerState,
        token: u32,
    ) {
        surface.with_pending_state(|state| {
            state.geometry = positioner.get_geometry();
            state.positioner = positioner;
        });
        self.unconstrain_popup(&surface);
        surface.send_repositioned(token);
    }

    fn grab(&mut self, _surface: PopupSurface, _seat: wl_seat::WlSeat, _serial: Serial) {}
}

impl SeatHandler for State {
    type KeyboardFocus = KbFocus;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<State> {
        &mut self.seat_state
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&KbFocus>) {
        let what = match focused {
            Some(KbFocus::X11(s)) => format!("x11 {} '{}'", s.window_id(), s.title()),
            Some(KbFocus::Wayland(_)) => "a Wayland window".to_string(),
            None => "nothing".to_string(),
        };
        info!("keyboard focus -> {what}");
        let dh = &self.display_handle;
        let client = focused.and_then(|f| f.wl_surface()).and_then(|s| {
            dh.get_client(smithay::reexports::wayland_server::Resource::id(&*s))
                .ok()
        });
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }
}

impl PointerConstraintsHandler for State {
    fn new_constraint(&mut self, surface: &WlSurface, pointer: &PointerHandle<Self>) {
        let over = pointer.current_focus().as_ref() == Some(surface);
        if over {
            self.activate_constraint(surface, pointer);
        }
    }

    fn remove_constraint(
        &mut self,
        surface: &WlSurface,
        _pointer: &PointerHandle<Self>,
        _remove: ConstraintRemove,
    ) {
        if let Some(idx) = self.window_for_surface(surface) {
            let window = self.windows[idx].id;
            info!(window, "app released the pointer");
            self.send_to_client(Control::PointerGrab {
                window,
                mode: PointerGrab::None,
            });
        }
    }
}

impl SelectionHandler for State {
    type SelectionUserData = ();

    fn new_selection(
        &mut self,
        ty: SelectionTarget,
        source: Option<SelectionSource>,
        _seat: Seat<Self>,
    ) {
        self.wayland_selection_changed(ty, source);
    }

    fn send_selection(
        &mut self,
        ty: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
        _seat: Seat<Self>,
        _user_data: &(),
    ) {
        self.serve_selection_to_wayland(ty, mime_type, fd);
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.primary_selection_state
    }
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.data_device_state
    }
}

impl DndGrabHandler for State {}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        _seat: Seat<Self>,
        _serial: Serial,
        _type: GrabType,
    ) {
        source.cancel();
    }
}

impl OutputHandler for State {}

smithay::delegate_dispatch2!(State);
