use std::{net::SocketAddr, sync::Arc, thread};

use anyhow::{Context, Result};
use proto::{
    Control,
    net::{Auth, TOKEN_LEN, token_matches},
};
use quinn::{Connection, Endpoint, RecvStream, SendStream, crypto::rustls::QuicServerConfig};
use smithay::reexports::calloop::channel::Sender as LoopSender;
use tokio::sync::mpsc::{Receiver, UnboundedReceiver, UnboundedSender};
use tracing::{error, info, warn};

use crate::{clock, encoder::EncodedFrame};

static AUTH_FILE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn remove_auth_file() {
    if let Some(path) = AUTH_FILE.get() {
        let _ = std::fs::remove_file(path);
    }
}

pub enum NetEvent {
    Connected(SocketAddr),
    Disconnected,
    Input(Control),
    Stopped,
}

struct StoppedGuard(LoopSender<NetEvent>);

impl Drop for StoppedGuard {
    fn drop(&mut self) {
        let _ = self.0.send(NetEvent::Stopped);
    }
}

pub fn spawn(
    listen: SocketAddr,
    authority: String,
    frames: UnboundedReceiver<EncodedFrame>,
    to_client: UnboundedReceiver<Control>,
    to_client_tx: UnboundedSender<Control>,
    audio: Receiver<Vec<u8>>,
    events: LoopSender<NetEvent>,
) {
    thread::Builder::new()
        .name("net".into())
        .spawn(move || {
            let _guard = StoppedGuard(events.clone());
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime");
            if let Err(err) = rt.block_on(serve(
                listen,
                &authority,
                frames,
                to_client,
                to_client_tx,
                audio,
                events,
            )) {
                error!("network stopped: {err:#}");
            }
        })
        .expect("spawn net thread");
}

fn server_config() -> Result<(quinn::ServerConfig, Auth)> {
    let cert = rcgen::generate_simple_self_signed(vec!["winstream".into()])?;
    let cert_der = cert.cert.der().clone();
    let auth = Auth::generate(&cert_der)?;
    let key_der = rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());
    let mut tls = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(vec![cert_der], key_der.into())?;
    tls.alpn_protocols = vec![proto::ALPN.to_vec()];
    let mut config = quinn::ServerConfig::with_crypto(Arc::new(QuicServerConfig::try_from(tls)?));
    config.transport_config(Arc::new(proto::net::transport()?));
    Ok((config, auth))
}

async fn authenticate(conn: &Connection, auth: &Auth) -> Result<(SendStream, RecvStream)> {
    let (send, mut recv) = conn.accept_bi().await?;
    let mut token = [0u8; TOKEN_LEN];
    recv.read_exact(&mut token).await?;
    if !token_matches(&auth.token, &token) {
        anyhow::bail!("wrong token");
    }
    Ok((send, recv))
}

async fn serve(
    listen: SocketAddr,
    authority: &str,
    mut frames: UnboundedReceiver<EncodedFrame>,
    mut to_client: UnboundedReceiver<Control>,
    to_client_tx: UnboundedSender<Control>,
    mut audio: Receiver<Vec<u8>>,
    events: LoopSender<NetEvent>,
) -> Result<()> {
    let (config, auth) = server_config()?;
    let endpoint = Endpoint::server(config, listen).context("bind QUIC endpoint")?;
    let path = auth.write(authority, listen.port())?;
    info!(path = %path.display(), "authority file written");
    let _ = AUTH_FILE.set(path);
    info!(%listen, "waiting for a client");
    while let Some(incoming) = endpoint.accept().await {
        let conn = match incoming.await {
            Ok(conn) => conn,
            Err(err) => {
                warn!("handshake failed: {err}");
                continue;
            }
        };
        let addr = conn.remote_address();
        let (send, recv) = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            authenticate(&conn, &auth),
        )
        .await
        {
            Ok(Ok(streams)) => streams,
            Ok(Err(err)) => {
                warn!(%addr, "client rejected: {err:#}");
                conn.close(1u32.into(), b"not authenticated");
                continue;
            }
            Err(_) => {
                warn!(%addr, "client rejected: no token within 3 s");
                conn.close(1u32.into(), b"not authenticated");
                continue;
            }
        };
        info!(%addr, "client connected");
        while frames.try_recv().is_ok() {}
        while to_client.try_recv().is_ok() {}
        while audio.try_recv().is_ok() {}
        let _ = events.send(NetEvent::Connected(addr));
        if let Err(err) = session(
            &conn,
            send,
            recv,
            &mut frames,
            &mut to_client,
            &to_client_tx,
            &mut audio,
            &events,
        )
        .await
        {
            info!(%addr, "client gone: {err:#}");
        }
        let _ = events.send(NetEvent::Disconnected);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn session(
    conn: &Connection,
    mut send: SendStream,
    mut recv: RecvStream,
    frames: &mut UnboundedReceiver<EncodedFrame>,
    to_client: &mut UnboundedReceiver<Control>,
    to_client_tx: &UnboundedSender<Control>,
    audio: &mut Receiver<Vec<u8>>,
    events: &LoopSender<NetEvent>,
) -> Result<()> {
    let audio_out = async {
        while let Some(packet) = audio.recv().await {
            match conn.send_datagram(bytes::Bytes::from(packet)) {
                Ok(()) => {}
                Err(quinn::SendDatagramError::ConnectionLost(e)) => return Err(e.into()),
                Err(err) => warn!("audio datagram: {err}"),
            }
        }
        std::future::pending::<()>().await;
        anyhow::Ok(())
    };
    let reader = async {
        let mut header = [0u8; Control::HEADER_LEN];
        let mut payload = Vec::new();
        loop {
            recv.read_exact(&mut header).await?;
            payload.resize(Control::payload_len(&header), 0);
            recv.read_exact(&mut payload).await?;
            match Control::decode(&header, &payload) {
                Some(Control::Ping { client_ns }) => {
                    let _ = to_client_tx.send(Control::Pong {
                        client_ns,
                        server_ns: clock::now_ns(),
                    });
                }
                Some(msg) => {
                    let _ = events.send(NetEvent::Input(msg));
                }
                None => warn!(tag = header[0], "unknown control message"),
            }
        }
        #[allow(unreachable_code)]
        anyhow::Ok(())
    };
    let writer = async {
        while let Some(msg) = to_client.recv().await {
            send.write_all(&msg.encode()).await?;
        }
        anyhow::Ok(())
    };
    let video = async {
        let mut stream = conn.open_uni().await?;
        while let Some(frame) = frames.recv().await {
            stream.write_all(&frame.header.encode()).await?;
            stream.write_all(&frame.data).await?;
        }
        anyhow::Ok(())
    };
    tokio::select! {
        r = reader => r.context("control stream (read)"),
        r = writer => r.context("control stream (write)"),
        r = audio_out => r.context("audio"),
        r = video => r.context("video stream"),
    }
}
