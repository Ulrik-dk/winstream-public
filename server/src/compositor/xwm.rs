use std::{borrow::Cow, process::Stdio};

use proto::Control;
use smithay::backend::input::KeyState;
use smithay::{
    desktop::Window,
    input::{
        Seat,
        keyboard::{KeyboardTarget, KeysymHandle, ModifiersState},
    },
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::{IsAlive, Logical, Rectangle, SERIAL_COUNTER, Serial},
    wayland::{
        seat::WaylandFocus,
        selection::SelectionTarget,
        xwayland_shell::{XWaylandShellHandler, XWaylandShellState},
    },
    xwayland::{
        X11Surface, X11Wm, XWayland, XWaylandEvent, XwmHandler,
        xwm::{Reorder, ResizeEdge, WmWindowProperty, XwmId},
    },
};
use tracing::{debug, info, warn};

use super::{State, StreamWindow};

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum KbFocus {
    Wayland(WlSurface),
    X11(X11Surface),
}

impl IsAlive for KbFocus {
    fn alive(&self) -> bool {
        match self {
            KbFocus::Wayland(s) => s.alive(),
            KbFocus::X11(s) => s.alive(),
        }
    }
}

impl WaylandFocus for KbFocus {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            KbFocus::Wayland(s) => Some(Cow::Borrowed(s)),
            KbFocus::X11(s) => s.wl_surface().map(Cow::Owned),
        }
    }
}

impl KeyboardTarget<State> for KbFocus {
    fn enter(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        keys: Vec<KeysymHandle<'_>>,
        serial: Serial,
    ) {
        match self {
            KbFocus::Wayland(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            KbFocus::X11(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<State>, data: &mut State, serial: Serial) {
        match self {
            KbFocus::Wayland(s) => KeyboardTarget::leave(s, seat, data, serial),
            KbFocus::X11(s) => KeyboardTarget::leave(s, seat, data, serial),
        }
    }

    fn key(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        key: KeysymHandle<'_>,
        state: KeyState,
        serial: Serial,
        time: smithay::backend::input::InputTime,
    ) {
        match self {
            KbFocus::Wayland(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            KbFocus::X11(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(
        &self,
        seat: &Seat<State>,
        data: &mut State,
        modifiers: ModifiersState,
        serial: Serial,
    ) {
        match self {
            KbFocus::Wayland(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            KbFocus::X11(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
        }
    }
}

pub struct Overlay {
    pub window: Window,
    pub owner: u32,
}

impl Overlay {
    pub fn x11(&self) -> &X11Surface {
        self.window.x11_surface().expect("x11 overlay")
    }

    pub fn position(&self) -> smithay::utils::Point<i32, Logical> {
        self.x11().last_configure().loc
    }
}

impl State {
    pub(super) fn start_xwayland(&mut self) -> bool {
        let spawned = XWayland::spawn(
            &self.display_handle,
            None,
            std::iter::empty::<(String, String)>(),
            std::iter::empty::<String>(),
            true,
            Stdio::null(),
            Stdio::inherit(),
            |_| (),
        );
        let (xwayland, client) = match spawned {
            Ok(x) => x,
            Err(err) => {
                warn!("XWayland not started, X11 apps won't work: {err}");
                return false;
            }
        };
        let dh = self.display_handle.clone();
        let inserted = self.loop_handle.insert_source(
            xwayland,
            move |event, _, state: &mut State| match event {
                XWaylandEvent::Ready {
                    x11_socket,
                    display_number,
                } => {
                    match X11Wm::start_wm(
                        state.loop_handle.clone(),
                        &dh,
                        x11_socket,
                        client.clone(),
                    ) {
                        Ok(wm) => {
                            info!(display = display_number, "XWayland ready");
                            state.xwm = Some(wm);
                            state.x_display = Some(display_number);
                        }
                        Err(err) => warn!("X11 window manager failed to start: {err}"),
                    }
                    state.apps_ready();
                }
                XWaylandEvent::Error => {
                    warn!("XWayland exited during startup; X11 apps won't work");
                    state.apps_ready();
                }
            },
        );
        if let Err(err) = inserted {
            warn!("XWayland event source: {err}");
            return false;
        }
        true
    }

    fn x11_window_index(&self, surface: &X11Surface) -> Option<usize> {
        self.windows
            .iter()
            .position(|w| w.window.x11_surface() == Some(surface))
    }

    fn is_x11_window(&self, id: u32) -> bool {
        self.window_index(id)
            .is_some_and(|i| self.windows[i].window.x11_surface().is_some())
    }

    fn overlay_owner(&self, surface: &X11Surface) -> Option<u32> {
        if let Some(parent) = surface.is_transient_for() {
            if let Some(w) = self.windows.iter().find(|w| {
                w.window
                    .x11_surface()
                    .is_some_and(|x| x.window_id() == parent)
            }) {
                return Some(w.id);
            }
            if let Some(o) = self.overlays.iter().find(|o| o.x11().window_id() == parent) {
                return Some(o.owner);
            }
        }
        self.pointer_window
            .filter(|&id| self.is_x11_window(id))
            .or(self.focused_window.filter(|&id| self.is_x11_window(id)))
            .or_else(|| {
                self.windows
                    .iter()
                    .rev()
                    .find(|w| w.window.x11_surface().is_some())
                    .map(|w| w.id)
            })
    }

    fn remove_overlay(&mut self, surface: &X11Surface) {
        let mut owners = Vec::new();
        self.overlays.retain(|o| {
            let keep = o.x11() != surface;
            if !keep {
                owners.push(o.owner);
            }
            keep
        });
        for owner in owners {
            self.mark_dirty(owner);
        }
    }

    fn x11_label(&self, surface: &X11Surface) -> String {
        let id = self
            .x11_window_index(surface)
            .map_or("-".to_string(), |i| self.windows[i].id.to_string());
        format!(
            "window={id} x11={} '{}'",
            surface.window_id(),
            surface.title()
        )
    }

    fn set_x11_title(&mut self, surface: &X11Surface) {
        let Some(idx) = self.x11_window_index(surface) else {
            return;
        };
        let w = &mut self.windows[idx];
        let (title, app_id) = (surface.title(), surface.class());
        if w.title == title && w.app_id == app_id {
            return;
        }
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

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwm.as_mut().expect("X11 window manager")
    }

    fn new_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        self.x11_selection_changed(selection, mime_types);
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        self.x11_selection_cleared(selection);
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        true
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: std::os::fd::OwnedFd,
    ) {
        self.serve_selection_to_x11(selection, mime_type, fd);
    }
    fn new_override_redirect_window(&mut self, _xwm: XwmId, _surface: X11Surface) {}

    fn map_window_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(geometry = ?surface.last_configure(), "x11: map request {}", self.x11_label(&surface));
        if let Err(err) = surface.set_mapped(true) {
            warn!(?err, "mapping X11 window");
            return;
        }
        let size = surface.last_configure().size;
        let _ = surface.configure(Rectangle::new((0, 0).into(), size));

        let id = self.next_window_id;
        self.next_window_id += 1;
        let (title, app_id) = (surface.title(), surface.class());
        info!(window = id, %title, %app_id, w = size.w, h = size.h, "X11 window opened");
        let w = StreamWindow {
            id,
            window: Window::new_x11_window(surface.clone()),
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
        self.focus_window(id, SERIAL_COUNTER.next_serial(), false);
        if surface.is_fullscreen() {
            self.send_to_client(Control::Fullscreen {
                window: id,
                on: true,
            });
        }
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        let Some(owner) = self.overlay_owner(&surface) else {
            debug!("override-redirect window with no X11 window to draw it in");
            return;
        };
        debug!(owner, geometry = ?surface.last_configure(), "X11 popup");
        self.overlays.push(Overlay {
            window: Window::new_x11_window(surface),
            owner,
        });
        self.mark_dirty(owner);
    }

    fn unmapped_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: unmapped {}{}",
            self.x11_label(&surface),
            if surface.is_override_redirect() {
                " (popup)"
            } else {
                ""
            }
        );
        if surface.is_override_redirect() {
            self.remove_overlay(&surface);
            return;
        }
        let _ = surface.set_mapped(false);
        if let Some(idx) = self.x11_window_index(&surface) {
            self.remove_window(idx);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: destroyed {}", self.x11_label(&surface));
        self.remove_overlay(&surface);
        if let Some(idx) = self.x11_window_index(&surface) {
            self.remove_window(idx);
        }
    }

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        reorder: Option<Reorder>,
    ) {
        info!(?x, ?y, ?w, ?h, ?reorder, from = ?surface.last_configure().size, "x11: configure request {}", self.x11_label(&surface));
        let mut geo = surface.last_configure();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        geo.loc = (0, 0).into();
        let _ = surface.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let owners: Vec<u32> = self
            .overlays
            .iter()
            .filter(|o| o.x11() == &surface)
            .map(|o| o.owner)
            .collect();
        for owner in owners {
            self.mark_dirty(owner);
        }
    }

    fn property_notify(&mut self, _xwm: XwmId, surface: X11Surface, property: WmWindowProperty) {
        info!(
            ?property,
            "x11: property changed {}",
            self.x11_label(&surface)
        );
        if matches!(property, WmWindowProperty::Title | WmWindowProperty::Class) {
            self.set_x11_title(&surface);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: fullscreen request {}", self.x11_label(&surface));
        if let Some(idx) = self.x11_window_index(&surface) {
            self.set_fullscreen(idx, true);
        }
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: unfullscreen request {}", self.x11_label(&surface));
        if let Some(idx) = self.x11_window_index(&surface) {
            self.set_fullscreen(idx, false);
        }
    }

    fn active_window_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _timestamp: u32,
        currently_active_window: Option<X11Surface>,
    ) {
        info!(
            current = currently_active_window.as_ref().map(|s| self.x11_label(s)),
            "x11: activate request {}",
            self.x11_label(&surface)
        );
        if let Some(idx) = self.x11_window_index(&surface) {
            let id = self.windows[idx].id;
            self.focus_window(id, SERIAL_COUNTER.next_serial(), false);
        }
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        surface: X11Surface,
        _button: u32,
        _edge: ResizeEdge,
    ) {
        info!("x11: resize request (ignored) {}", self.x11_label(&surface));
    }
    fn move_request(&mut self, _xwm: XwmId, surface: X11Surface, _button: u32) {
        info!("x11: move request (ignored) {}", self.x11_label(&surface));
    }

    fn minimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: minimize request, iconic until focused {}",
            self.x11_label(&surface)
        );
        if let Err(err) = surface.set_hidden(true) {
            warn!("x11: can't minimize: {err}");
        }
    }
    fn unminimize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: unminimize request {}", self.x11_label(&surface));
        if let Err(err) = surface.set_hidden(false) {
            warn!("x11: can't unminimize: {err}");
        }
    }
    fn maximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: maximize request (ignored) {}",
            self.x11_label(&surface)
        );
    }
    fn unmaximize_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: unmaximize request (ignored) {}",
            self.x11_label(&surface)
        );
    }
    fn above_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: above request (ignored) {}", self.x11_label(&surface));
    }
    fn unabove_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: unabove request (ignored) {}",
            self.x11_label(&surface)
        );
    }
    fn below_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!("x11: below request (ignored) {}", self.x11_label(&surface));
    }
    fn unbelow_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: unbelow request (ignored) {}",
            self.x11_label(&surface)
        );
    }
    fn demands_attention_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: demands-attention request (ignored) {}",
            self.x11_label(&surface)
        );
    }
    fn undemands_attention_request(&mut self, _xwm: XwmId, surface: X11Surface) {
        info!(
            "x11: undemands-attention request (ignored) {}",
            self.x11_label(&surface)
        );
    }

    fn disconnected(&mut self, _xwm: XwmId) {
        warn!("XWayland went away");
        self.xwm = None;
        self.x_display = None;
    }
}
