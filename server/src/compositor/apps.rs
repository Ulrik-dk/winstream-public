use std::{
    io::{Read, Write},
    os::unix::{
        net::{UnixListener, UnixStream},
        process::CommandExt,
    },
    path::PathBuf,
    process::{Child, Command},
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use smithay::reexports::calloop::{Interest, Mode as LoopMode, PostAction, generic::Generic};
use tracing::{info, warn};

use super::{State, group_alive};

pub struct App {
    pub name: String,
    pub child: Option<Child>,
    pub group: i32,
}

pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    dir.join("winstream-launch.sock")
}

fn request(words: &[String]) -> Result<String> {
    let path = socket_path();
    let mut stream = UnixStream::connect(&path).with_context(|| {
        format!(
            "no running server ({} not found or not answering)",
            path.display()
        )
    })?;
    for word in words {
        stream.write_all(word.as_bytes())?;
        stream.write_all(b"\0")?;
    }
    stream.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply.trim().to_string())
}

pub fn launch(command: &[String]) -> Result<()> {
    let mut words = vec!["launch".to_string()];
    words.extend_from_slice(command);
    let reply = request(&words)?;
    println!("{reply}");
    if reply.starts_with("error") {
        bail!("server could not start {}", command[0]);
    }
    Ok(())
}

pub fn stop_apps_request() -> Result<()> {
    let reply = request(&["stop".to_string()])?;
    let Some(groups) = reply.strip_prefix("stopping ") else {
        println!("{reply}");
        if reply.starts_with("error") {
            bail!("server could not stop its apps");
        }
        return Ok(());
    };
    let mut left: Vec<i32> = groups
        .split_whitespace()
        .filter_map(|g| g.parse().ok())
        .collect();
    println!(
        "stopping apps (process groups {groups}); waiting up to 30 s for them to save and exit"
    );
    for _ in 0..300 {
        left.retain(|&g| unsafe { libc::kill(-g, 0) } == 0);
        if left.is_empty() {
            println!("all apps stopped");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    for &g in &left {
        println!("process group {g} still running after 30 s; killing it");
        unsafe { libc::kill(-g, libc::SIGKILL) };
    }
    Ok(())
}

impl State {
    pub(super) fn listen_for_launches(&mut self) -> Result<()> {
        let path = socket_path();
        let _ = std::fs::remove_file(&path);
        let listener =
            UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
        listener.set_nonblocking(true)?;
        self.loop_handle
            .insert_source(
                Generic::new(listener, Interest::READ, LoopMode::Level),
                |_, listener, state: &mut State| {
                    while let Ok((stream, _)) = listener.accept() {
                        state.on_launch_request(stream);
                    }
                    Ok(PostAction::Continue)
                },
            )
            .map_err(|e| anyhow!("launch socket source: {e}"))?;
        info!(path = %path.display(), "launch socket");
        self.launch_socket = Some(path);
        Ok(())
    }

    fn on_launch_request(&mut self, mut stream: UnixStream) {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let mut buf = Vec::new();
        let reply = match stream.read_to_end(&mut buf) {
            Err(err) => format!("error: reading request: {err}"),
            Ok(_) => {
                let mut words: Vec<String> = buf
                    .split(|&b| b == 0)
                    .filter(|a| !a.is_empty())
                    .map(|a| String::from_utf8_lossy(a).into_owned())
                    .collect();
                let verb = if words.is_empty() {
                    String::new()
                } else {
                    words.remove(0)
                };
                match verb.as_str() {
                    "launch" if !words.is_empty() => self.request_app(words),
                    "launch" => "error: empty command".into(),
                    "stop" => self.term_apps(),
                    other => format!("error: unknown request {other:?}"),
                }
            }
        };
        let _ = stream.write_all(format!("{reply}\n").as_bytes());
    }

    pub(super) fn request_app(&mut self, command: Vec<String>) -> String {
        if !self.apps_ready {
            info!(?command, "app queued until XWayland is ready");
            self.queued_apps.push(command);
            return "queued: starts once XWayland is ready".into();
        }
        match self.spawn_app(&command) {
            Ok(pid) => format!("started {} (pid {pid})", command[0]),
            Err(err) => {
                warn!("{err:#}");
                format!("error: {err:#}")
            }
        }
    }

    fn term_apps(&mut self) -> String {
        self.queued_apps.clear();
        let groups: Vec<i32> = self
            .apps
            .iter()
            .map(|a| a.group)
            .filter(|&g| group_alive(g))
            .collect();
        if groups.is_empty() {
            return "no apps".into();
        }
        info!(?groups, "stopping apps on request");
        for &g in &groups {
            unsafe { libc::kill(-g, libc::SIGTERM) };
        }
        let list: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
        format!("stopping {}", list.join(" "))
    }

    pub(super) fn apps_ready(&mut self) {
        self.apps_ready = true;
        self.share_displays_with_session();
        for command in std::mem::take(&mut self.queued_apps) {
            if let Err(err) = self.spawn_app(&command) {
                warn!("{err:#}");
                if self.end_with_apps && self.apps.is_empty() {
                    self.finish(Err(err));
                }
            }
        }
    }

    fn share_displays_with_session(&self) {
        let own_runtime = format!("/run/user/{}", unsafe { libc::getuid() });
        let is_session = std::env::var("XDG_RUNTIME_DIR").is_ok_and(|d| d == own_runtime);
        if !is_session || std::env::var_os("WINSTREAM_NO_SESSION_ENV").is_some() {
            return;
        }
        let mut vars = vec![format!(
            "WAYLAND_DISPLAY={}",
            self.socket_name.to_string_lossy()
        )];
        if let Some(n) = self.x_display {
            vars.push(format!("DISPLAY=:{n}"));
        }
        std::thread::spawn(move || {
            let update = Command::new("dbus-update-activation-environment")
                .arg("--systemd")
                .args(&vars)
                .status();
            let restart = Command::new("systemctl")
                .args(["--user", "try-restart", "xdg-desktop-portal-gtk.service"])
                .status();
            match (update, restart) {
                (Ok(u), Ok(r)) if u.success() && r.success() => {
                    info!(
                        ?vars,
                        "session services (desktop portal) now use our displays"
                    )
                }
                (u, r) => warn!(
                    ?vars,
                    ?u,
                    ?r,
                    "couldn't point session services at our displays"
                ),
            }
        });
    }

    fn spawn_app(&mut self, command: &[String]) -> Result<u32> {
        info!(socket = ?self.socket_name, display = ?self.x_display, ?command, "starting app");
        let mut app = Command::new(&command[0]);
        app.args(&command[1..])
            .env("WAYLAND_DISPLAY", &self.socket_name);
        match self.x_display {
            Some(n) => app.env("DISPLAY", format!(":{n}")),
            None => app.env_remove("DISPLAY"),
        };
        if let Some(sink) = &self.pulse_sink {
            app.env("PULSE_SINK", sink);
        }
        app.process_group(0);
        unsafe {
            app.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::pthread_sigmask(libc::SIG_SETMASK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        let child = app
            .spawn()
            .with_context(|| format!("starting {}", command[0]))?;
        let pid = child.id();
        self.apps.push(App {
            name: command[0].clone(),
            child: Some(child),
            group: pid as i32,
        });
        Ok(pid)
    }

    pub(super) fn reap_apps(&mut self) {
        for app in &mut self.apps {
            if let Some(child) = &mut app.child
                && let Ok(Some(status)) = child.try_wait()
            {
                info!(app = %app.name, %status, "launched process exited");
                app.child = None;
            }
        }
        let before = self.apps.len();
        self.apps.retain(|app| {
            let alive = app.child.is_some() || group_alive(app.group);
            if !alive {
                info!(app = %app.name, "all of the app's processes exited");
            }
            alive
        });
        if self.end_with_apps && before > 0 && self.apps.is_empty() && self.queued_apps.is_empty() {
            info!("all apps exited; ending the session");
            self.finish(Ok(()));
        }
    }
}

pub fn stop_apps(apps: &mut Vec<App>) {
    let groups: Vec<i32> = apps
        .iter()
        .map(|a| a.group)
        .filter(|&g| group_alive(g))
        .collect();
    if !groups.is_empty() {
        info!(
            ?groups,
            "stopping apps (waiting up to 30 s for them to save and exit)"
        );
        for &g in &groups {
            unsafe { libc::kill(-g, libc::SIGTERM) };
        }
        let mut left = groups;
        for _ in 0..300 {
            std::thread::sleep(Duration::from_millis(100));
            left.retain(|&g| group_alive(g));
            if left.is_empty() {
                break;
            }
        }
        for g in left {
            warn!(group = g, "app still running after 30 s; killing it");
            unsafe { libc::kill(-g, libc::SIGKILL) };
        }
    }
    for app in apps.drain(..) {
        if let Some(mut child) = app.child {
            let _ = child.wait();
        }
    }
}
