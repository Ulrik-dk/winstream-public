use std::{
    collections::{HashMap, VecDeque},
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result};
use proto::{Control, FrameHeader, net::Auth};
use tracing::{error, info};
use winit::event_loop::EventLoopProxy;

use crate::{Latest, UserEvent, clock, decode};

#[derive(Default)]
pub struct ClockSync {
    pub offset_ns: AtomicI64,
    pub rtt_ns: AtomicI64,
    pub audio_delay_us: AtomicI64,
    pub audio: AudioTotals,
}

#[derive(Default)]
pub struct AudioTotals {
    pub packets: AtomicI64,
    pub to_output_us: AtomicI64,
    pub output_us: AtomicI64,
    pub concealed: AtomicI64,
    pub dropped: AtomicI64,
}

impl AudioTotals {
    pub fn take(&self) -> Option<(f64, f64, i64, i64)> {
        let n = self.packets.swap(0, Ordering::Relaxed);
        let to_output = self.to_output_us.swap(0, Ordering::Relaxed);
        let output = self.output_us.swap(0, Ordering::Relaxed);
        let concealed = self.concealed.swap(0, Ordering::Relaxed);
        let dropped = self.dropped.swap(0, Ordering::Relaxed);
        (n > 0).then(|| {
            (
                to_output as f64 / n as f64 / 1000.0,
                output as f64 / n as f64 / 1000.0,
                concealed,
                dropped,
            )
        })
    }
}

pub struct Received {
    pub header: FrameHeader,
    pub data: Vec<u8>,
    pub received_ns: u64,
}

pub type ControlSender = tokio::sync::mpsc::UnboundedSender<Control>;

pub fn spawn(
    servers: Vec<String>,
    ssh: Option<String>,
    authority: String,
    sync: Arc<ClockSync>,
    proxy: EventLoopProxy<UserEvent>,
    latest: Latest,
) -> ControlSender {
    let (control_tx, control_rx) = tokio::sync::mpsc::unbounded_channel();
    let pings = control_tx.clone();
    let (audio_tx, audio_rx) = mpsc::channel();
    crate::audio::spawn(audio_rx, sync.clone());
    thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            if let Err(err) = rt.block_on(run(
                &servers,
                ssh.as_deref(),
                &authority,
                sync,
                pings,
                control_rx,
                &proxy,
                &latest,
                audio_tx,
            )) {
                error!("connection ended: {err:#}");
            }
            let _ = proxy.send_event(UserEvent::Ended);
        })
        .expect("spawn net thread");
    control_tx
}

fn resolve(server: &str) -> Result<SocketAddr> {
    let with_port = if server.contains(':') {
        server.to_string()
    } else {
        format!("{server}:{}", proto::DEFAULT_PORT)
    };
    with_port
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .with_context(|| format!("no IPv4 address for {with_port}"))
}

async fn connect_first(
    servers: &[String],
    ssh: Option<&str>,
    authority: &str,
) -> Result<(quinn::Connection, quinn::SendStream, quinn::RecvStream)> {
    let mut auths: HashMap<u16, Auth> = HashMap::new();
    let mut last_err = None;
    for (i, server) in servers.iter().enumerate() {
        let wait = if i + 1 == servers.len() {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(1)
        };
        let attempt = async {
            let addr = resolve(server)?;
            let auth = match auths.get(&addr.port()) {
                Some(auth) => auth.clone(),
                None => {
                    let auth = Auth::fetch(ssh, authority, addr.port())?;
                    auths.insert(addr.port(), auth.clone());
                    auth
                }
            };
            info!(%addr, "connecting");
            proto::net::connect(addr, &auth, wait).await
        };
        match attempt.await {
            Ok(streams) => return Ok(streams),
            Err(err) => {
                info!(server, "not reachable: {err:#}");
                last_err = Some(err);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no server address given")))
}

#[allow(clippy::too_many_arguments)]
async fn run(
    servers: &[String],
    ssh: Option<&str>,
    authority: &str,
    sync: Arc<ClockSync>,
    pings: ControlSender,
    mut outgoing: tokio::sync::mpsc::UnboundedReceiver<Control>,
    proxy: &EventLoopProxy<UserEvent>,
    latest: &Latest,
    audio: mpsc::Sender<bytes::Bytes>,
) -> Result<()> {
    let decoders: std::cell::RefCell<HashMap<u32, mpsc::Sender<Received>>> = Default::default();
    let (conn, mut send, mut recv) = connect_first(servers, ssh, authority).await?;
    info!(addr = %conn.remote_address(), rtt = ?conn.rtt(), "connected");

    let writer = async {
        while let Some(msg) = outgoing.recv().await {
            send.write_all(&msg.encode()).await?;
        }
        anyhow::Ok(())
    };
    let pinger = async {
        let mut sent = 0u32;
        loop {
            if pings
                .send(Control::Ping {
                    client_ns: clock::now_ns(),
                })
                .is_err()
            {
                return anyhow::Ok(());
            }
            sent += 1;
            let wait = if sent < 20 { 50 } else { 1000 };
            tokio::time::sleep(Duration::from_millis(wait)).await;
        }
    };
    let ponger = async {
        let mut samples: VecDeque<(i64, i64)> = VecDeque::new();
        let mut header = [0u8; Control::HEADER_LEN];
        let mut payload = Vec::new();
        loop {
            recv.read_exact(&mut header).await?;
            payload.resize(Control::payload_len(&header), 0);
            recv.read_exact(&mut payload).await?;
            let now = clock::now_ns() as i64;
            let msg = Control::decode(&header, &payload);
            if let Some(Control::WindowClosed { window }) = &msg {
                decoders.borrow_mut().remove(window);
            }
            if let Some(Control::Pong {
                client_ns,
                server_ns,
            }) = msg
            {
                let rtt = now - client_ns as i64;
                let midpoint = client_ns as i64 + rtt / 2;
                samples.push_back((rtt, server_ns as i64 - midpoint));
                if samples.len() > 16 {
                    samples.pop_front();
                }
                let &(best_rtt, offset) = samples.iter().min_by_key(|s| s.0).unwrap();
                sync.offset_ns.store(offset, Ordering::Relaxed);
                sync.rtt_ns.store(best_rtt, Ordering::Relaxed);
            } else if let Some(msg) = msg {
                let _ = proxy.send_event(UserEvent::Server(msg));
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let video = async {
        let mut stream = conn.accept_uni().await?;
        let mut header = [0u8; FrameHeader::LEN];
        loop {
            stream.read_exact(&mut header).await?;
            let header = FrameHeader::decode(&header);
            let mut data = vec![0u8; header.len as usize];
            stream.read_exact(&mut data).await?;
            let received_ns = clock::now_ns();
            let mut decoders = decoders.borrow_mut();
            let decoder = decoders.entry(header.window_id).or_insert_with(|| {
                let (tx, rx) = mpsc::channel();
                decode::spawn(header.window_id, rx, proxy.clone(), latest.clone());
                tx
            });
            let _ = decoder.send(Received {
                header,
                data,
                received_ns,
            });
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let audio_in = async {
        loop {
            let packet = conn.read_datagram().await?;
            let _ = audio.send(packet);
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    tokio::select! {
        r = audio_in => r.context("audio"),
        r = writer => r.context("control send"),
        r = pinger => r.context("pings"),
        r = ponger => r.context("control receive"),
        r = video => r.context("video"),
    }
}
