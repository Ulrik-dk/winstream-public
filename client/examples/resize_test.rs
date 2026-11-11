use std::{
    net::{SocketAddr, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result};
use proto::{Control, FrameHeader, net::Auth};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let server = args
        .get(1)
        .context("usage: resize_test HOST:PORT [STEPS] [MS_PER_STEP]")?;
    let steps: u32 = args.get(2).map_or(Ok(60), |s| s.parse())?;
    let step_ms: u64 = args.get(3).map_or(Ok(500), |s| s.parse())?;

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
    send.write_all(
        &Control::Display {
            width: 2560,
            height: 1440,
            refresh_mhz: 60_000,
        }
        .encode(),
    )
    .await?;

    let frames = Arc::new(AtomicU64::new(0));
    let video = {
        let conn = conn.clone();
        let frames = frames.clone();
        tokio::spawn(async move {
            let mut stream = conn.accept_uni().await?;
            let mut header = [0u8; FrameHeader::LEN];
            loop {
                stream.read_exact(&mut header).await?;
                let header = FrameHeader::decode(&header);
                let mut data = vec![0u8; header.len as usize];
                stream.read_exact(&mut data).await?;
                frames.fetch_add(1, Ordering::Relaxed);
            }
            #[allow(unreachable_code)]
            anyhow::Ok(())
        })
    };
    tokio::spawn(async move { while conn.read_datagram().await.is_ok() {} });

    let mut header = [0u8; Control::HEADER_LEN];
    let mut payload = Vec::new();
    let window = loop {
        recv.read_exact(&mut header).await?;
        payload.resize(Control::payload_len(&header), 0);
        recv.read_exact(&mut payload).await?;
        if let Some(Control::WindowOpened {
            window,
            width,
            height,
            title,
            ..
        }) = Control::decode(&header, &payload)
        {
            println!("window {window} '{title}' {width}x{height}");
            break window;
        }
    };
    tokio::spawn(async move {
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
                Some(Control::Pong { .. }) | None => {}
                Some(msg) => println!("  server: {msg:?}"),
            }
        }
    });
    tokio::time::sleep(Duration::from_secs(2)).await;

    for step in 0..steps {
        let t = step as f64 / 10.0;
        let width = (1500.0 + 1000.0 * t.sin()).round() as u32 + step % 7;
        let height = (900.0 + 500.0 * (t * 0.7).cos()).round() as u32 + step % 5;
        let width = width.clamp(200, 2560);
        let height = height.clamp(150, 1440);
        let before = frames.load(Ordering::Relaxed);
        send.write_all(
            &Control::Resize {
                window,
                width,
                height,
            }
            .encode(),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(step_ms)).await;
        let got = frames.load(Ordering::Relaxed) - before;
        println!("step {step:3} {width:4}x{height:<4} frames {got}");
        if video.is_finished() {
            println!("video stream ended");
            break;
        }
    }
    Ok(())
}
