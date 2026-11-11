use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc, Mutex,
        atomic::{AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use proto::{Control, FrameHeader, net::Auth};

fn now_ns() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_nanos() as u64
}

#[derive(Default)]
struct Counts {
    frames: u32,
    bytes: u64,
    delay_sum_us: u64,
    last_frame_id: u64,
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let usage = "usage: rate_test HOST:PORT SECONDS SCHEDULE [WxH]";
    let server = args.get(1).context(usage)?;
    let seconds: u64 = args.get(2).context(usage)?.parse()?;
    let schedule: Vec<(f64, f64)> = args
        .get(3)
        .context(usage)?
        .split(',')
        .map(|p| {
            let (t, r) = p
                .split_once(':')
                .context("schedule entry is start_s:mbit")?;
            anyhow::Ok((t.parse()?, r.parse()?))
        })
        .collect::<Result<_>>()?;
    let size: (u32, u32) = match args.get(4) {
        Some(s) => {
            let (w, h) = s.split_once('x').context("WxH")?;
            (w.parse()?, h.parse()?)
        }
        None => (2560, 1440),
    };
    let schedule = Arc::new(schedule);
    let limit_at = move |t: f64| {
        schedule
            .iter()
            .rev()
            .find(|(start, _)| t >= *start)
            .map_or(0.0, |s| s.1)
    };

    let addr: SocketAddr = server
        .to_socket_addrs()?
        .find(|a| a.is_ipv4())
        .context("no IPv4 address")?;
    let authority = std::env::var("WINSTREAM_AUTHORITY")
        .unwrap_or_else(|_| proto::net::DEFAULT_AUTHORITY.to_string());
    let auth = Auth::fetch(
        std::env::var("WINSTREAM_SSH").ok().as_deref(),
        &authority,
        addr.port(),
    )?;
    let (conn, mut send, mut recv) =
        proto::net::connect(addr, &auth, Duration::from_secs(5)).await?;
    let (tx, mut outgoing) = tokio::sync::mpsc::unbounded_channel::<Control>();
    tokio::spawn(async move {
        while let Some(msg) = outgoing.recv().await {
            if send.write_all(&msg.encode()).await.is_err() {
                return;
            }
        }
    });
    let _ = tx.send(Control::Display {
        width: 2560,
        height: 1440,
        refresh_mhz: 60_000,
    });
    let start = Instant::now();

    let offset = Arc::new(AtomicI64::new(i64::MIN));
    let window = Arc::new(AtomicI64::new(0));
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            for i in 0u64.. {
                if tx
                    .send(Control::Ping {
                        client_ns: now_ns(),
                    })
                    .is_err()
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(if i < 20 { 50 } else { 1000 })).await;
            }
        });
    }
    {
        let (offset, window, tx) = (offset.clone(), window.clone(), tx.clone());
        tokio::spawn(async move {
            let mut header = [0u8; Control::HEADER_LEN];
            let mut payload = Vec::new();
            let mut best_rtt = i64::MAX;
            loop {
                if recv.read_exact(&mut header).await.is_err() {
                    println!("control stream ended");
                    return;
                }
                payload.resize(Control::payload_len(&header), 0);
                if recv.read_exact(&mut payload).await.is_err() {
                    return;
                }
                match Control::decode(&header, &payload) {
                    Some(Control::Pong {
                        client_ns,
                        server_ns,
                    }) => {
                        let now = now_ns() as i64;
                        let rtt = now - client_ns as i64;
                        if rtt < best_rtt {
                            best_rtt = rtt;
                            offset.store(
                                server_ns as i64 - (client_ns as i64 + rtt / 2),
                                Ordering::Relaxed,
                            );
                        }
                    }
                    Some(Control::WindowOpened {
                        window: id, title, ..
                    }) if window.load(Ordering::Relaxed) == 0 => {
                        println!("window {id} '{title}'; asking for {}x{}", size.0, size.1);
                        window.store(id as i64, Ordering::Relaxed);
                        let _ = tx.send(Control::Resize {
                            window: id,
                            width: size.0,
                            height: size.1,
                        });
                    }
                    _ => {}
                }
            }
        });
    }

    let counts = Arc::new(Mutex::new(Counts::default()));
    {
        let (conn, counts, offset, window) =
            (conn.clone(), counts.clone(), offset.clone(), window.clone());
        let limit_at = limit_at.clone();
        tokio::spawn(async move {
            let mut stream = conn.accept_uni().await?;
            let mut header = [0u8; FrameHeader::LEN];
            let mut budget = 0.0f64;
            let mut last = Instant::now();
            loop {
                stream.read_exact(&mut header).await?;
                let header = FrameHeader::decode(&header);
                let mut data = vec![0u8; header.len as usize];
                stream.read_exact(&mut data).await?;
                let received_ns = now_ns() as i64;
                let limit = limit_at(start.elapsed().as_secs_f64());
                if limit > 0.0 {
                    let now = Instant::now();
                    budget = (budget + (now - last).as_secs_f64() * limit * 1e6 / 8.0)
                        .min(limit * 1e6 / 8.0 / 10.0);
                    last = now;
                    budget -= (FrameHeader::LEN + data.len()) as f64;
                    if budget < 0.0 {
                        let wait = -budget / (limit * 1e6 / 8.0);
                        tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                    }
                } else {
                    budget = 0.0;
                    last = Instant::now();
                }
                if header.window_id as i64 != window.load(Ordering::Relaxed) {
                    continue;
                }
                let off = offset.load(Ordering::Relaxed);
                let mut c = counts.lock().unwrap();
                c.frames += 1;
                c.bytes += data.len() as u64;
                if off != i64::MIN {
                    let net = received_ns - (header.encoded_ns as i64 - off);
                    c.delay_sum_us += (net.max(0) / 1000) as u64;
                }
                c.last_frame_id = header.frame_id;
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        });
    }
    tokio::spawn(async move { while conn.read_datagram().await.is_ok() {} });

    let mut second = Counts::default();
    let mut ticks = 0u64;
    while start.elapsed().as_secs() < seconds {
        tokio::time::sleep(Duration::from_millis(proto::FEEDBACK_INTERVAL_MS)).await;
        let c = std::mem::take(&mut *counts.lock().unwrap());
        let id = window.load(Ordering::Relaxed) as u32;
        if id != 0 && c.frames > 0 {
            let _ = tx.send(Control::Feedback {
                window: id,
                received: c.frames,
                shown: c.frames,
                last_frame_id: c.last_frame_id,
                net_delay_us: (c.delay_sum_us / c.frames as u64) as u32,
                latency_us: (c.delay_sum_us / c.frames as u64) as u32,
            });
        }
        second.frames += c.frames;
        second.bytes += c.bytes;
        second.delay_sum_us += c.delay_sum_us;
        ticks += 1;
        if ticks % (1000 / proto::FEEDBACK_INTERVAL_MS) == 0 {
            let t = start.elapsed().as_secs_f64();
            let limit = limit_at(t);
            println!(
                "t={t:5.1} limit={:>5} fps={:3} mbit={:5.1} net_ms={:6.1}",
                if limit > 0.0 {
                    format!("{limit}")
                } else {
                    "none".into()
                },
                second.frames,
                second.bytes as f64 * 8.0 / 1e6,
                if second.frames > 0 {
                    second.delay_sum_us as f64 / second.frames as f64 / 1000.0
                } else {
                    0.0
                },
            );
            second = Counts::default();
        }
    }
    Ok(())
}
