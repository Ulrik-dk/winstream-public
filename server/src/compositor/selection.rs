use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    thread,
    time::{Duration, Instant},
};

use proto::Control;
use smithay::{
    reexports::calloop::{
        channel::Sender,
        timer::{TimeoutAction, Timer},
    },
    wayland::selection::{
        SelectionSource, SelectionTarget,
        data_device::{
            clear_data_device_selection, current_data_device_selection_userdata,
            request_data_device_client_selection, set_data_device_selection,
        },
        primary_selection::{
            clear_primary_selection, current_primary_selection_userdata,
            request_primary_client_selection, set_primary_selection,
        },
    },
};
use tracing::{debug, info, warn};

use super::State;

const TEXT_MIME: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
const READ_TIMEOUT: Duration = Duration::from_secs(2);
const AFTER_RELEASE: Duration = Duration::from_millis(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Wayland,
    X11,
}

pub struct Selections {
    tx: Sender<(SelectionTarget, String)>,
    owner: [Option<(Owner, String)>; 2],
    sent: [Option<String>; 2],
    laptop: [Option<String>; 2],
}

impl Selections {
    pub fn new(tx: Sender<(SelectionTarget, String)>) -> Self {
        Selections {
            tx,
            owner: [None, None],
            sent: [None, None],
            laptop: [None, None],
        }
    }
}

fn index(target: SelectionTarget) -> usize {
    match target {
        SelectionTarget::Primary => 0,
        SelectionTarget::Clipboard => 1,
    }
}

fn text_mime(mime_types: &[String]) -> Option<String> {
    TEXT_MIME
        .iter()
        .find(|m| mime_types.iter().any(|t| t == *m))
        .map(|m| m.to_string())
}

impl State {
    pub(super) fn wayland_selection_changed(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
    ) {
        let mime_types = source.as_ref().map(|s| s.mime_types());
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.new_selection(target, mime_types.clone())
        {
            warn!(?target, "can't offer the selection to X11 apps: {err}");
        }
        let owner = source.is_some().then_some(Owner::Wayland);
        self.selection_changed(target, owner, &mime_types.unwrap_or_default());
    }

    pub(super) fn x11_selection_changed(
        &mut self,
        target: SelectionTarget,
        mime_types: Vec<String>,
    ) {
        match target {
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types.clone(), ())
            }
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), ())
            }
        }
        self.selection_changed(target, Some(Owner::X11), &mime_types);
    }

    pub(super) fn x11_selection_cleared(&mut self, target: SelectionTarget) {
        match target {
            SelectionTarget::Primary
                if current_primary_selection_userdata(&self.seat).is_some() =>
            {
                clear_primary_selection(&self.display_handle, &self.seat)
            }
            SelectionTarget::Clipboard
                if current_data_device_selection_userdata(&self.seat).is_some() =>
            {
                clear_data_device_selection(&self.display_handle, &self.seat)
            }
            _ => {}
        }
        self.selection_changed(target, None, &[]);
    }

    pub(super) fn selection_from_laptop(&mut self, primary: bool, text: String) {
        let target = if primary {
            SelectionTarget::Primary
        } else {
            SelectionTarget::Clipboard
        };
        let i = index(target);
        if self.selections.laptop[i].as_deref() == Some(text.as_str()) {
            return;
        }
        info!(?target, bytes = text.len(), "selection from the laptop");
        let mime_types: Vec<String> = TEXT_MIME.iter().map(|m| m.to_string()).collect();
        match target {
            SelectionTarget::Primary => {
                set_primary_selection(&self.display_handle, &self.seat, mime_types.clone(), ())
            }
            SelectionTarget::Clipboard => {
                set_data_device_selection(&self.display_handle, &self.seat, mime_types.clone(), ())
            }
        }
        if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.new_selection(target, Some(mime_types))
        {
            warn!(
                ?target,
                "can't offer the laptop's selection to X11 apps: {err}"
            );
        }
        self.selections.owner[i] = None;
        self.selections.sent[i] = Some(text.clone());
        self.selections.laptop[i] = Some(text);
    }

    pub(super) fn serve_selection_to_wayland(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) {
        if let Some(text) = self.selections.laptop[index(target)].clone() {
            write_text(fd, text);
        } else if let Some(xwm) = self.xwm.as_mut()
            && let Err(err) = xwm.send_selection(target, mime, fd)
        {
            debug!(?target, "can't pass the X11 selection on: {err}");
        }
    }

    pub(super) fn serve_selection_to_x11(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: OwnedFd,
    ) {
        if let Some(text) = self.selections.laptop[index(target)].clone() {
            write_text(fd, text);
            return;
        }
        let result = match target {
            SelectionTarget::Primary => {
                request_primary_client_selection(&self.seat, mime, fd).map_err(|e| e.to_string())
            }
            SelectionTarget::Clipboard => {
                request_data_device_client_selection(&self.seat, mime, fd)
                    .map_err(|e| e.to_string())
            }
        };
        if let Err(err) = result {
            debug!(?target, "can't pass the Wayland selection on: {err}");
        }
    }

    fn selection_changed(
        &mut self,
        target: SelectionTarget,
        owner: Option<Owner>,
        mime_types: &[String],
    ) {
        let entry = owner.and_then(|o| text_mime(mime_types).map(|m| (o, m)));
        debug!(?target, ?entry, "selection changed");
        self.selections.owner[index(target)] = entry;
        self.selections.laptop[index(target)] = None;
        self.loop_handle
            .insert_idle(move |state| state.read_selection(target));
    }

    pub(super) fn resend_selections(&mut self) {
        self.selections.sent = [None, None];
        for target in [SelectionTarget::Primary, SelectionTarget::Clipboard] {
            self.loop_handle
                .insert_idle(move |state| state.read_selection(target));
        }
    }

    pub(super) fn reread_primary_soon(&mut self) {
        if self.selections.owner[0].is_none() {
            return;
        }
        let _ =
            self.loop_handle
                .insert_source(Timer::from_duration(AFTER_RELEASE), |_, _, state| {
                    state.read_selection(SelectionTarget::Primary);
                    TimeoutAction::Drop
                });
    }

    fn read_selection(&mut self, target: SelectionTarget) {
        if !self.streaming {
            return;
        }
        let Some((owner, mime)) = self.selections.owner[index(target)].clone() else {
            return;
        };
        let (read, write) = match pipe() {
            Ok(fds) => fds,
            Err(err) => {
                warn!("selection pipe: {err}");
                return;
            }
        };
        let asked = match (owner, target) {
            (Owner::Wayland, SelectionTarget::Primary) => {
                request_primary_client_selection(&self.seat, mime, write).map_err(|e| e.to_string())
            }
            (Owner::Wayland, SelectionTarget::Clipboard) => {
                request_data_device_client_selection(&self.seat, mime, write)
                    .map_err(|e| e.to_string())
            }
            (Owner::X11, _) => match self.xwm.as_mut() {
                Some(xwm) => xwm
                    .send_selection(target, mime, write)
                    .map_err(|e| e.to_string()),
                None => Err("no X11 window manager".into()),
            },
        };
        if let Err(err) = asked {
            debug!(?target, "can't read selection: {err}");
            return;
        }
        let tx = self.selections.tx.clone();
        thread::spawn(move || {
            if let Some(text) = read_text(read) {
                let _ = tx.send((target, text));
            }
        });
    }

    pub(super) fn selection_read(&mut self, target: SelectionTarget, text: String) {
        let i = index(target);
        if text.is_empty() || self.selections.sent[i].as_deref() == Some(text.as_str()) {
            return;
        }
        info!(?target, bytes = text.len(), "selection to client");
        self.send_to_client(Control::Selection {
            primary: target == SelectionTarget::Primary,
            text: text.clone(),
        });
        self.selections.sent[i] = Some(text);
    }
}

fn write_text(fd: OwnedFd, text: String) {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags & !libc::O_NONBLOCK);
    }
    thread::spawn(move || {
        let _ = File::from(fd).write_all(text.as_bytes());
    });
}

fn pipe() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

fn read_text(fd: OwnedFd) -> Option<String> {
    let raw = fd.as_raw_fd();
    unsafe {
        let flags = libc::fcntl(raw, libc::F_GETFL);
        libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
    }
    let mut file = File::from(fd);
    let deadline = Instant::now() + READ_TIMEOUT;
    let mut data = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            debug!("selection read timed out");
            break;
        }
        let mut pfd = libc::pollfd {
            fd: raw,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut pfd, 1, left.as_millis() as i32) } <= 0 {
            continue;
        }
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                data.extend_from_slice(&buf[..n]);
                if data.len() > Control::MAX_SELECTION {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    let mut text = String::from_utf8_lossy(&data).into_owned();
    if text.len() > Control::MAX_SELECTION {
        let mut end = Control::MAX_SELECTION;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Some(text)
}
