mod audio;
mod clock;
mod compositor;
mod encoder;
mod gpu_copy;
mod net;

use std::{
    net::SocketAddr,
    sync::atomic::{AtomicBool, Ordering},
};

use anyhow::{Context, Result, bail};

pub struct Config {
    pub listen: SocketAddr,
    pub authority: String,
    pub width: i32,
    pub height: i32,
    pub settings: encoder::Settings,
    pub command: Vec<String>,
}

const USAGE: &str = "usage:\n  server stream [--listen ADDR] [--authority PATH] [--size WxH] [--fps N] [--latency-ms MS]\n      [--start-bpp BPP] [--max-bpp BPP] [--min-mbit MBIT] [-- COMMAND [ARGS...]]\n  server launch -- COMMAND [ARGS...]\n  server stop-apps";

pub static STOP: AtomicBool = AtomicBool::new(false);

fn handle_signals() {
    unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for sig in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
            libc::sigaddset(&mut set, sig);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        std::thread::Builder::new()
            .name("signals".into())
            .spawn(move || {
                loop {
                    let mut sig = 0;
                    if libc::sigwait(&set, &mut sig) == 0 {
                        STOP.store(true, Ordering::Relaxed);
                        tracing::info!(sig, "signal: shutting down");
                    }
                }
            })
            .expect("spawn signal thread");
    }
}

fn watch_hangup() {
    std::thread::Builder::new()
        .name("hangup".into())
        .spawn(|| {
            loop {
                let mut fds = [1, 2].map(|fd| libc::pollfd {
                    fd,
                    events: 0,
                    revents: 0,
                });
                let r = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as _, -1) };
                if r > 0
                    && fds
                        .iter()
                        .any(|f| f.revents & (libc::POLLHUP | libc::POLLERR) != 0)
                {
                    STOP.store(true, Ordering::Relaxed);
                    unsafe {
                        let null = libc::open(c"/dev/null".as_ptr(), libc::O_WRONLY);
                        if null >= 0 {
                            libc::dup2(null, 1);
                            libc::dup2(null, 2);
                            libc::close(null);
                        }
                    }
                    tracing::info!("the command that started us is gone: shutting down");
                    return;
                }
                if r < 0
                    && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
                {
                    return;
                }
            }
        })
        .expect("spawn hangup thread");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("stop-apps") {
        return compositor::apps::stop_apps_request();
    }
    if args.get(1).map(String::as_str) == Some("launch") {
        let command: Vec<String> = args
            .iter()
            .skip(2)
            .skip_while(|a| *a == "--")
            .cloned()
            .collect();
        if command.is_empty() {
            bail!("{USAGE}");
        }
        return compositor::apps::launch(&command);
    }
    handle_signals();
    watch_hangup();
    clock::init();
    let _log_guards = init_logging();
    let config = parse_args().context(USAGE)?;
    compositor::run(config)
}

fn init_logging() -> Vec<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "info,smithay=warn".into());
    let mut guards = Vec::new();
    let (stderr, guard) = tracing_appender::non_blocking(std::io::stderr());
    guards.push(guard);
    let mut layers = vec![tracing_subscriber::fmt::layer().with_writer(stderr).boxed()];
    let file = log_file();
    let path = file.as_ref().map(|(_, p)| p.clone());
    if let Some((file, _)) = file {
        let (file, guard) = tracing_appender::non_blocking(file);
        guards.push(guard);
        layers.push(
            tracing_subscriber::fmt::layer()
                .with_writer(file)
                .with_ansi(false)
                .boxed(),
        );
    }
    tracing_subscriber::registry()
        .with(filter)
        .with(layers)
        .init();
    if let Some(path) = path {
        tracing::info!(path = %path.display(), pid = std::process::id(), "log file");
    }
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        tracing::error!(
            thread = thread.name().unwrap_or("?"),
            backtrace = %std::backtrace::Backtrace::force_capture(),
            "panic: {info}"
        );
        default_hook(info);
    }));
    guards
}

fn log_file() -> Option<(std::fs::File, std::path::PathBuf)> {
    let dir = std::path::PathBuf::from(std::env::var_os("HOME")?).join(".local/state/winstream");
    std::fs::create_dir_all(&dir).ok()?;
    let mut old: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "log"))
        .collect();
    old.sort();
    for p in old.iter().rev().skip(19) {
        let _ = std::fs::remove_file(p);
    }
    let path = dir.join(format!("server-{}.log", local_timestamp()));
    let file = std::fs::File::create(&path).ok()?;
    Some((file, path))
}

fn local_timestamp() -> String {
    unsafe {
        let t = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            tm.tm_year + 1900,
            tm.tm_mon + 1,
            tm.tm_mday,
            tm.tm_hour,
            tm.tm_min,
            tm.tm_sec
        )
    }
}

fn parse_args() -> Result<Config> {
    let mut args = std::env::args().skip(1);
    let sub = args.next().context("missing subcommand")?;
    let mut listen: SocketAddr = ([0, 0, 0, 0], proto::DEFAULT_PORT).into();
    let mut authority = proto::net::DEFAULT_AUTHORITY.to_string();
    let (mut width, mut height) = (1280, 720);
    let mut fps: u32 = 60;
    let mut latency_ms: u64 = 15;
    let mut start_bpp: f64 = 0.08;
    let mut max_bpp: f64 = 1.0;
    let mut min_mbit: f64 = 2.0;
    if sub != "stream" {
        bail!("unknown subcommand {sub}");
    }
    let mut command = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => listen = args.next().context("--listen ADDR")?.parse()?,
            "--authority" => authority = args.next().context("--authority PATH")?,
            "--size" => {
                let s = args.next().context("--size WxH")?;
                let (w, h) = s.split_once('x').context("--size WxH")?;
                (width, height) = (w.parse()?, h.parse()?);
            }
            "--fps" => fps = args.next().context("--fps N")?.parse()?,
            "--latency-ms" => latency_ms = args.next().context("--latency-ms MS")?.parse()?,
            "--start-bpp" => start_bpp = args.next().context("--start-bpp BPP")?.parse()?,
            "--max-bpp" => max_bpp = args.next().context("--max-bpp BPP")?.parse()?,
            "--min-mbit" => min_mbit = args.next().context("--min-mbit MBIT")?.parse()?,
            "--" => {
                command.extend(args.by_ref());
            }
            other => bail!("unexpected argument {other}"),
        }
    }
    if !(start_bpp > 0.0 && max_bpp >= start_bpp && min_mbit > 0.0) {
        bail!("need 0 < --start-bpp <= --max-bpp and --min-mbit > 0");
    }
    Ok(Config {
        listen,
        authority,
        width,
        height,
        settings: encoder::Settings {
            fps: fps.max(1),
            latency: std::time::Duration::from_millis(latency_ms.max(1)),
            start_bpp,
            max_bpp,
            min_bitrate: (min_mbit * 1e6) as u32,
        },
        command,
    })
}
