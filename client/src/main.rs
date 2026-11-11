mod audio;
mod decode;
mod net;
mod render;

use std::{
    collections::{HashMap, HashSet},
    rc::Rc,
    sync::{Arc, Mutex, OnceLock, atomic::Ordering},
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use proto::{Control, FrameHeader, PointerGrab};
use tracing::{info, warn};
use winit::{
    application::ApplicationHandler,
    dpi::PhysicalSize,
    event::{DeviceEvent, DeviceId, ElementState, MouseButton, MouseScrollDelta, WindowEvent},
    event_loop::{ActiveEventLoop, EventLoop},
    platform::{
        scancode::PhysicalKeyExtScancode,
        x11::{WindowAttributesExtX11, WindowType},
    },
    window::{CursorGrabMode, Fullscreen, Window, WindowId},
};

mod clock {
    use super::*;
    static START: OnceLock<Instant> = OnceLock::new();
    pub fn now_ns() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_nanos() as u64
    }
}

pub enum Picture {
    Gpu(decode::GpuFrame),
    Cpu(Vec<u32>),
}

pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub picture: Picture,
    pub header: FrameHeader,
    pub received_ns: u64,
    pub decoded_ns: u64,
    pub skipped: u32,
}

pub type Latest = Arc<Mutex<HashMap<u32, Decoded>>>;

pub enum UserEvent {
    FrameReady(u32),
    Server(Control),
    Ended,
}

fn main() -> Result<()> {
    clock::now_ns();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let usage = "usage: client [--ssh SSH_DEST] [--authority PATH] SERVER[:PORT]...";
    let mut args = std::env::args().skip(1);
    let mut ssh = None;
    let mut authority = proto::net::DEFAULT_AUTHORITY.to_string();
    let mut servers = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--ssh" => ssh = Some(args.next().context(usage)?),
            "--authority" => authority = args.next().context(usage)?,
            _ if !arg.starts_with('-') => servers.push(arg),
            _ => anyhow::bail!("{usage}"),
        }
    }
    if servers.is_empty() {
        anyhow::bail!("{usage}");
    }
    let server = servers.join(" or ");

    if std::env::var("WINSTREAM_CPU_PATH").is_ok_and(|v| v == "1") {
        decode::GPU_PATH.store(false, Ordering::Relaxed);
        info!("CPU path: decoded frames are copied to memory");
    }
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let sync = Arc::new(net::ClockSync::default());
    let latest: Latest = Default::default();
    let control = net::spawn(
        servers,
        ssh,
        authority,
        sync.clone(),
        event_loop.create_proxy(),
        latest.clone(),
    );
    let _ = control.send(local_keymap());

    let mut app = App {
        server,
        sync,
        control,
        latest,
        windows: HashMap::new(),
        by_winit: HashMap::new(),
        info: HashMap::new(),
        keys_down: HashSet::new(),
        buttons_down: HashSet::new(),
        grab: None,
        focused: None,
        reactivate_on_click: None,
        display_sent: None,
        overlay: std::env::var("WINSTREAM_OVERLAY").map_or(true, |v| v != "0"),
        overlay_shown: true,
        renderer: None,
        overlay_checked: None,
        selections_known: Default::default(),
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn local_keymap() -> Control {
    let out = std::process::Command::new("setxkbmap")
        .arg("-query")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let field = |name: &str| {
        out.lines()
            .find_map(|l| {
                l.strip_prefix(name)?
                    .strip_prefix(':')
                    .map(|v| v.trim().to_string())
            })
            .unwrap_or_default()
    };
    let mut layout = field("layout");
    if layout.is_empty() {
        layout = "us".into();
    }
    Control::Keymap {
        rules: field("rules"),
        model: field("model"),
        layout,
        variant: field("variant"),
        options: field("options"),
    }
}

const BTN_MIDDLE: u32 = 0x112;

fn evdev_button(button: MouseButton) -> Option<u32> {
    Some(match button {
        MouseButton::Left => 0x110,
        MouseButton::Right => 0x111,
        MouseButton::Middle => BTN_MIDDLE,
        MouseButton::Back => 0x116,
        MouseButton::Forward => 0x115,
        MouseButton::Other(_) => return None,
    })
}

#[derive(Default, Clone)]
struct WindowInfo {
    title: String,
    app_id: String,
    parent: u32,
}

struct ClientWindow {
    window: Rc<Window>,
    gl: render::WindowGl,
    capture: bool,
    requested: Option<(u32, u32)>,
    last_frame: Option<Decoded>,
    stats: Stats,
    title_stats: String,
    feedback: Feedback,
}

#[derive(Default)]
struct Feedback {
    since: Option<Instant>,
    received: u32,
    shown: u32,
    delay_sum_us: u64,
    latency_sum_us: u64,
    last_frame_id: u64,
}

impl Feedback {
    fn add(&mut self, window: u32, f: &Decoded, shown: bool, offset: i64) -> Option<Control> {
        let arrived = 1 + f.skipped;
        self.received += arrived;
        self.shown += shown as u32;
        let net_ns = f.received_ns as i64 - (f.header.encoded_ns as i64 - offset);
        self.delay_sum_us += (net_ns.max(0) / 1000) as u64 * arrived as u64;
        if shown {
            let latency_ns = clock::now_ns() as i64 - (f.header.encoded_ns as i64 - offset);
            self.latency_sum_us += (latency_ns.max(0) / 1000) as u64;
        }
        self.last_frame_id = f.header.frame_id;
        let since = *self.since.get_or_insert_with(Instant::now);
        if since.elapsed() < Duration::from_millis(proto::FEEDBACK_INTERVAL_MS) {
            return None;
        }
        let report = Control::Feedback {
            window,
            received: self.received,
            shown: self.shown,
            last_frame_id: self.last_frame_id,
            net_delay_us: (self.delay_sum_us / self.received.max(1) as u64).min(u32::MAX as u64)
                as u32,
            latency_us: (self.latency_sum_us / self.shown.max(1) as u64).min(u32::MAX as u64)
                as u32,
        };
        *self = Feedback {
            since: Some(Instant::now()),
            ..Feedback::default()
        };
        Some(report)
    }
}

struct App {
    server: String,
    sync: Arc<net::ClockSync>,
    control: net::ControlSender,
    latest: Latest,
    windows: HashMap<u32, ClientWindow>,
    by_winit: HashMap<WindowId, u32>,
    info: HashMap<u32, WindowInfo>,
    keys_down: HashSet<u32>,
    buttons_down: HashSet<u32>,
    grab: Option<(u32, PointerGrab)>,
    focused: Option<u32>,
    reactivate_on_click: Option<u32>,
    display_sent: Option<(u32, u32, u32)>,
    overlay: bool,
    overlay_shown: bool,
    overlay_checked: Option<Instant>,
    renderer: Option<render::Renderer>,
    selections_known: SelectionsKnown,
}

type SelectionsKnown = Arc<Mutex<[Option<String>; 2]>>;

fn read_local_selection(primary: bool) -> Option<String> {
    let which = if primary { "primary" } else { "clipboard" };
    let out = std::process::Command::new("timeout")
        .args([
            "0.5",
            "xclip",
            "-o",
            "-selection",
            which,
            "-t",
            "UTF8_STRING",
        ])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() {
        return None;
    }
    let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
    if text.len() > Control::MAX_SELECTION {
        let mut end = Control::MAX_SELECTION;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Some(text)
}

fn offer_local_selections(
    primaries: &[bool],
    known: &SelectionsKnown,
    control: &net::ControlSender,
) {
    for &primary in primaries {
        let Some(text) = read_local_selection(primary) else {
            continue;
        };
        let mut known = known.lock().unwrap();
        let slot = &mut known[if primary { 0 } else { 1 }];
        if slot.as_deref() != Some(text.as_str()) {
            *slot = Some(text.clone());
            drop(known);
            info!(primary, bytes = text.len(), "selection to the server");
            let _ = control.send(Control::Selection { primary, text });
        }
    }
}

fn overlay_off_flag() -> std::path::PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").unwrap_or_else(|| "/tmp".into());
    std::path::Path::new(&dir).join("winstream-overlay-off")
}

fn set_local_selection(primary: bool, text: String) {
    std::thread::spawn(move || {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let which = if primary { "primary" } else { "clipboard" };
        match Command::new("xclip")
            .args(["-selection", which])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(mut child) => {
                if let Some(mut stdin) = child.stdin.take() {
                    let _ = stdin.write_all(text.as_bytes());
                }
                let _ = child.wait();
            }
            Err(err) => warn!("can't set the {which} selection (is xclip installed?): {err}"),
        }
    });
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

    fn device_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _device: DeviceId,
        event: DeviceEvent,
    ) {
        if let DeviceEvent::MouseMotion { delta } = event
            && let Some((window, _)) = self.grab
            && self.focused == Some(window)
        {
            self.send(Control::RelativeMotion {
                dx: delta.0,
                dy: delta.1,
            });
        }
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        winit_id: WindowId,
        event: WindowEvent,
    ) {
        let Some(&id) = self.by_winit.get(&winit_id) else {
            return;
        };
        match event {
            WindowEvent::CloseRequested => self.send(Control::Close { window: id }),
            WindowEvent::Resized(size) => {
                let Some(w) = self.windows.get_mut(&id) else {
                    return;
                };
                let size = (size.width, size.height);
                let showing = w.last_frame.as_ref().map(|f| (f.width, f.height));
                if size.0 > 0 && size.1 > 0 && Some(size) != showing && Some(size) != w.requested {
                    w.requested = Some(size);
                    let _ = self.control.send(Control::Resize {
                        window: id,
                        width: size.0,
                        height: size.1,
                    });
                }
                self.redraw(id);
            }
            WindowEvent::RedrawRequested => {
                self.redraw(id);
            }
            WindowEvent::Focused(true) => {
                self.focused = Some(id);
                self.reactivate_on_click = Some(id);
                self.send_display(id);
                self.send(Control::Focus { window: id });
                self.apply_grab();
                let (known, control) = (self.selections_known.clone(), self.control.clone());
                std::thread::spawn(move || {
                    offer_local_selections(&[true, false], &known, &control)
                });
            }
            WindowEvent::Focused(false) => {
                if self.focused == Some(id) {
                    self.focused = None;
                }
                self.release_all();
                self.apply_grab();
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                if is_synthetic || event.repeat {
                    return;
                }
                let Some(code) = event.physical_key.to_scancode() else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                if pressed {
                    self.keys_down.insert(code);
                } else if !self.keys_down.remove(&code) {
                    return;
                }
                self.send(Control::Key { code, pressed });
            }
            WindowEvent::CursorMoved { position, .. } => {
                if self.grab == Some((id, PointerGrab::Locked)) {
                    return;
                }
                self.send(Control::PointerMotion {
                    window: id,
                    x: position.x,
                    y: position.y,
                });
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let Some(button) = evdev_button(button) else {
                    return;
                };
                let pressed = state == ElementState::Pressed;
                if pressed && self.reactivate_on_click.take() == Some(id) {
                    self.send(Control::Focus { window: id });
                }
                if pressed && self.focused == Some(id) && self.grab.is_some_and(|(g, _)| g == id) {
                    self.apply_grab();
                }
                if pressed && button == BTN_MIDDLE {
                    offer_local_selections(&[true], &self.selections_known, &self.control);
                }
                if pressed {
                    self.buttons_down.insert(button);
                } else {
                    self.buttons_down.remove(&button);
                }
                self.send(Control::PointerButton { button, pressed });
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let msg = match delta {
                    MouseScrollDelta::LineDelta(x, y) => Control::Scroll {
                        dx: -x as f64 * 15.0,
                        dy: -y as f64 * 15.0,
                        steps_x: (-x * 120.0) as i32,
                        steps_y: (-y * 120.0) as i32,
                    },
                    MouseScrollDelta::PixelDelta(p) => Control::Scroll {
                        dx: -p.x,
                        dy: -p.y,
                        steps_x: 0,
                        steps_y: 0,
                    },
                };
                self.send(msg);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::FrameReady(id) => {
                let frame = self.latest.lock().unwrap().remove(&id);
                if let Some(frame) = frame {
                    self.on_frame(event_loop, frame);
                }
            }
            UserEvent::Server(msg) => self.on_server(event_loop, msg),
            UserEvent::Ended => {
                info!("stream ended");
                event_loop.exit();
            }
        }
    }
}

impl App {
    fn send(&self, msg: Control) {
        let _ = self.control.send(msg);
    }

    fn send_display(&mut self, id: u32) {
        let Some(m) = self
            .windows
            .get(&id)
            .and_then(|w| w.window.current_monitor())
        else {
            return;
        };
        let size = m.size();
        let screen = (
            size.width,
            size.height,
            m.refresh_rate_millihertz().unwrap_or(60_000),
        );
        if self.display_sent != Some(screen) && screen.0 > 0 {
            info!(
                width = screen.0,
                height = screen.1,
                refresh_mhz = screen.2,
                "local display"
            );
            self.send(Control::Display {
                width: screen.0,
                height: screen.1,
                refresh_mhz: screen.2,
            });
            self.display_sent = Some(screen);
        }
    }

    fn apply_grab(&mut self) {
        for (&id, w) in &self.windows {
            let mode = match self.grab {
                Some((g, mode)) if g == id && self.focused == Some(id) => mode,
                _ => PointerGrab::None,
            };
            let (grab, visible) = match mode {
                PointerGrab::None => (CursorGrabMode::None, true),
                PointerGrab::Confined => (CursorGrabMode::Confined, true),
                PointerGrab::Locked => (CursorGrabMode::Locked, false),
            };
            let mut result = w.window.set_cursor_grab(grab);
            if result.is_err() && grab == CursorGrabMode::Locked {
                result = w.window.set_cursor_grab(CursorGrabMode::Confined);
            }
            if let Err(err) = result {
                warn!(
                    window = id,
                    ?mode,
                    "can't grab the pointer, click the window to retry: {err}"
                );
            }
            w.window.set_cursor_visible(visible);
        }
    }

    fn release_all(&mut self) {
        for code in std::mem::take(&mut self.keys_down) {
            self.send(Control::Key {
                code,
                pressed: false,
            });
        }
        for button in std::mem::take(&mut self.buttons_down) {
            self.send(Control::PointerButton {
                button,
                pressed: false,
            });
        }
    }

    fn on_server(&mut self, event_loop: &ActiveEventLoop, msg: Control) {
        match msg {
            Control::WindowOpened {
                window,
                parent,
                width,
                height,
                title,
                app_id,
            } => {
                info!(window, %title, %app_id, parent, "window opened");
                self.info.insert(
                    window,
                    WindowInfo {
                        title,
                        app_id,
                        parent,
                    },
                );
                if width > 0 && height > 0 {
                    self.ensure_window(event_loop, window, width, height);
                }
            }
            Control::WindowTitle {
                window,
                title,
                app_id,
            } => {
                if let Some(w) = self.windows.get(&window) {
                    w.window
                        .set_title(&title_with_stats(&title, &w.title_stats));
                }
                let info = self.info.entry(window).or_default();
                info.title = title;
                info.app_id = app_id;
            }
            Control::PointerGrab { window, mode } => {
                info!(window, ?mode, "pointer grab");
                self.grab = (mode != PointerGrab::None).then_some((window, mode));
                self.apply_grab();
            }
            Control::Selection { primary, text } => {
                info!(primary, bytes = text.len(), "selection from the server");
                self.selections_known.lock().unwrap()[if primary { 0 } else { 1 }] =
                    Some(text.clone());
                set_local_selection(primary, text);
            }
            Control::Fullscreen { window, on } => {
                if let Some(w) = self.windows.get(&window) {
                    info!(window, on, "fullscreen");
                    w.window
                        .set_fullscreen(on.then_some(Fullscreen::Borderless(None)));
                }
            }
            Control::WindowClosed { window } => {
                info!(window, "window closed");
                self.info.remove(&window);
                if let Some(w) = self.windows.remove(&window) {
                    self.by_winit.remove(&w.window.id());
                    if let Some(renderer) = &mut self.renderer {
                        renderer.close_window(w.gl);
                    }
                }
            }
            _ => {}
        }
    }

    fn ensure_window(&mut self, event_loop: &ActiveEventLoop, id: u32, width: u32, height: u32) {
        if self.windows.contains_key(&id) {
            return;
        }
        let info = self.info.get(&id).cloned().unwrap_or_default();
        let title = if info.title.is_empty() {
            format!("{} (winstream)", self.server)
        } else {
            info.title.clone()
        };
        let class = if info.app_id.is_empty() {
            "winstream".to_string()
        } else {
            info.app_id.clone()
        };
        let mut attrs = Window::default_attributes()
            .with_title(title)
            .with_inner_size(PhysicalSize::new(width, height))
            .with_name(class.clone(), class);
        if std::env::var("WINSTREAM_FULLSCREEN").is_ok_and(|v| v == "1") {
            attrs = attrs.with_fullscreen(Some(Fullscreen::Borderless(None)));
        }
        if info.parent != 0 {
            attrs = attrs.with_x11_window_type(vec![WindowType::Dialog]);
        }
        let created = match &mut self.renderer {
            Some(renderer) => renderer.new_window(event_loop, attrs),
            None => render::Renderer::new(event_loop, attrs).map(|(renderer, window, gl)| {
                self.renderer = Some(renderer);
                (window, gl)
            }),
        };
        let (window, gl) = match created {
            Ok((w, gl)) => (Rc::new(w), gl),
            Err(err) => {
                tracing::error!(id, "create window: {err:#}");
                return;
            }
        };
        let first = self.windows.is_empty();
        self.by_winit.insert(window.id(), id);
        self.windows.insert(
            id,
            ClientWindow {
                window,
                gl,
                capture: std::env::var_os("WINSTREAM_SCREENSHOT_DIR").is_some(),
                requested: None,
                last_frame: None,
                stats: Stats::default(),
                title_stats: String::new(),
                feedback: Feedback::default(),
            },
        );
        if first {
            self.send_display(id);
        }
    }

    fn on_frame(&mut self, event_loop: &ActiveEventLoop, frame: Decoded) {
        let id = frame.header.window_id;
        if !self.info.contains_key(&id) && !self.windows.contains_key(&id) {
            return;
        }
        self.ensure_window(event_loop, id, frame.width, frame.height);
        let Some(w) = self.windows.get_mut(&id) else {
            return;
        };
        let showing = w.last_frame.as_ref().map(|f| (f.width, f.height));
        let new = (frame.width, frame.height);
        if showing.is_some() && showing != Some(new) && w.requested != Some(new) {
            let _ = w.window.request_inner_size(PhysicalSize::new(new.0, new.1));
        }
        w.last_frame = Some(frame);
        let shown = self.redraw(id);
        let offset = self.sync.offset_ns.load(Ordering::Relaxed);
        if let Some(w) = self.windows.get_mut(&id)
            && let Some(frame) = &w.last_frame
            && let Some(report) = w.feedback.add(id, frame, shown, offset)
        {
            let _ = self.control.send(report);
        }
        if shown {
            let presented_ns = clock::now_ns();
            let offset = self.sync.offset_ns.load(Ordering::Relaxed);
            let rtt = self.sync.rtt_ns.load(Ordering::Relaxed);
            let w = self.windows.get_mut(&id).unwrap();
            let frame = w.last_frame.as_ref().unwrap();
            if let Some(second) = w.stats.add(frame, presented_ns, offset, rtt) {
                let (fps, latency_ms, dropped) = (second.fps, second.latency, second.dropped());
                let title = self.info.get(&id).map(|i| i.title.as_str()).unwrap_or("");
                let audio_ms = self.sync.audio_delay_us.load(Ordering::Relaxed) / 1000;
                info!("[{id} {title}] {}  |  audio {audio_ms} ms", second.line());
                if let Ok(path) = std::env::var("WINSTREAM_STATS_CSV") {
                    second.append_csv(&path, id, frame.width, frame.height, self.sync.audio.take());
                }
                let mut stats =
                    format!("{fps} fps · {latency_ms:.0} ms · {:.1} Mbit/s", second.mbit);
                if dropped > 0 {
                    stats += &format!(" · {dropped} dropped");
                }
                let audio_us = self.sync.audio_delay_us.load(Ordering::Relaxed);
                if audio_us > 0 {
                    stats += &format!(" · audio {} ms", audio_us / 1000);
                }
                w.title_stats = stats;
                w.window.set_title(&title_with_stats(title, &w.title_stats));
                w.capture = std::env::var_os("WINSTREAM_SCREENSHOT_DIR").is_some();
            }
        }
    }

    fn overlay_on(&mut self) -> bool {
        if !self.overlay {
            return false;
        }
        if self
            .overlay_checked
            .is_none_or(|t| t.elapsed() > Duration::from_millis(500))
        {
            self.overlay_shown = !overlay_off_flag().exists();
            self.overlay_checked = Some(Instant::now());
        }
        self.overlay_shown
    }

    fn redraw(&mut self, id: u32) -> bool {
        let show_overlay = self.overlay_on();
        let Some(renderer) = self.renderer.as_mut() else {
            return false;
        };
        let Some(w) = self.windows.get_mut(&id) else {
            return false;
        };
        let Some(frame) = &w.last_frame else {
            return false;
        };
        let overlay_text = show_overlay
            .then(|| w.title_stats.clone())
            .filter(|t| !t.is_empty());
        match renderer.draw(
            &mut w.gl,
            &w.window,
            frame,
            overlay_text.as_deref(),
            w.capture,
        ) {
            Ok(shot) => {
                if let Some(rgb) = shot
                    && let Ok(dir) = std::env::var("WINSTREAM_SCREENSHOT_DIR")
                {
                    save_screenshot(&dir, id, frame.width, frame.height, &rgb);
                }
                w.capture = false;
                true
            }
            Err(err) => {
                if matches!(frame.picture, Picture::Gpu(_)) {
                    warn!("can't draw a GPU frame, copying frames from now on: {err:#}");
                    decode::GPU_PATH.store(false, Ordering::Relaxed);
                } else {
                    warn!("draw: {err:#}");
                }
                false
            }
        }
    }
}

fn title_with_stats(title: &str, stats: &str) -> String {
    match (title.is_empty(), stats.is_empty()) {
        (_, true) => title.to_string(),
        (true, false) => stats.to_string(),
        (false, false) => format!("{title} — {stats}"),
    }
}

fn save_screenshot(dir: &str, id: u32, width: u32, height: u32, rgb: &[u8]) {
    let path = format!("{dir}/window-{id}.png");
    if let Err(err) = image::save_buffer(&path, rgb, width, height, image::ColorType::Rgb8) {
        tracing::warn!(%path, "screenshot: {err}");
    }
}

#[derive(Default)]
struct Stats {
    window_start: Option<u64>,
    last_frame_id: Option<u64>,
    dropped_server: u64,
    dropped_client: u64,
    n: u32,
    total: Vec<f64>,
    encode: f64,
    network: f64,
    decode: f64,
    display: f64,
    bytes: u64,
}

impl Stats {
    fn add(&mut self, f: &Decoded, presented_ns: u64, offset: i64, rtt: i64) -> Option<Second> {
        let ms = |ns: i64| ns as f64 / 1e6;
        let id = f.header.frame_id;
        if let Some(last) = self.last_frame_id
            && id > last
        {
            let gap = id - last - 1;
            let client = (f.skipped as u64).min(gap);
            self.dropped_client += client;
            self.dropped_server += gap - client;
        }
        self.last_frame_id = Some(id);
        let commit = f.header.commit_ns as i64 - offset;
        let encoded = f.header.encoded_ns as i64 - offset;
        self.total.push(ms(presented_ns as i64 - commit));
        self.encode += ms(f.header.encoded_ns as i64 - f.header.commit_ns as i64);
        self.network += ms(f.received_ns as i64 - encoded);
        self.decode += ms(f.decoded_ns as i64 - f.received_ns as i64);
        self.display += ms(presented_ns as i64 - f.decoded_ns as i64);
        self.bytes += f.header.len as u64;
        self.n += 1;

        let start = *self.window_start.get_or_insert(presented_ns);
        if presented_ns - start < 1_000_000_000 {
            return None;
        }
        let n = self.n as f64;
        let mut total = std::mem::take(&mut self.total);
        total.sort_by(f64::total_cmp);
        let avg = total.iter().sum::<f64>() / n;
        let p95 = total[((total.len() as f64 * 0.95) as usize).min(total.len() - 1)];
        let second = Second {
            fps: self.n,
            latency: avg,
            p95,
            server: self.encode / n,
            net: self.network / n,
            decode: self.decode / n,
            show: self.display / n,
            dropped_server: self.dropped_server,
            dropped_client: self.dropped_client,
            mbit: self.bytes as f64 * 8.0 / 1e6,
            rtt: ms(rtt),
        };
        *self = Stats {
            last_frame_id: self.last_frame_id,
            ..Stats::default()
        };
        Some(second)
    }
}

struct Second {
    fps: u32,
    latency: f64,
    p95: f64,
    server: f64,
    net: f64,
    decode: f64,
    show: f64,
    dropped_server: u64,
    dropped_client: u64,
    mbit: f64,
    rtt: f64,
}

impl Second {
    fn dropped(&self) -> u64 {
        self.dropped_server + self.dropped_client
    }

    fn line(&self) -> String {
        format!(
            "{} fps  latency {:.1} ms (p95 {:.1})  = server {:.1} + net {:.1} + decode {:.1} + show {:.1}  |  dropped {} (server {}, client {})  |  {:.1} Mbit/s  rtt {:.1} ms",
            self.fps,
            self.latency,
            self.p95,
            self.server,
            self.net,
            self.decode,
            self.show,
            self.dropped(),
            self.dropped_server,
            self.dropped_client,
            self.mbit,
            self.rtt,
        )
    }

    fn append_csv(
        &self,
        path: &str,
        window: u32,
        width: u32,
        height: u32,
        audio: Option<(f64, f64, i64, i64)>,
    ) {
        use std::io::Write;
        let new = !std::path::Path::new(path).exists();
        let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        else {
            return;
        };
        if new {
            let _ = writeln!(
                f,
                "unix_s,window,width,height,fps,latency_ms,p95_ms,server_ms,net_ms,decode_ms,show_ms,dropped_server,dropped_client,mbit_s,rtt_ms,audio_to_output_ms,audio_output_ms,audio_concealed,audio_dropped"
            );
        }
        let unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0.0, |d| d.as_secs_f64());
        let _ = writeln!(
            f,
            "{unix:.3},{window},{width},{height},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{},{},{:.3},{:.3},{}",
            self.fps,
            self.latency,
            self.p95,
            self.server,
            self.net,
            self.decode,
            self.show,
            self.dropped_server,
            self.dropped_client,
            self.mbit,
            self.rtt,
            match audio {
                Some((to_output, output, concealed, dropped)) =>
                    format!("{to_output:.3},{output:.3},{concealed},{dropped}"),
                None => ",,,".to_string(),
            },
        );
    }
}
