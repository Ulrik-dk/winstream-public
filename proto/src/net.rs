use std::{
    io::Write,
    net::SocketAddr,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::PathBuf,
    process::Command,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use quinn::{Connection, Endpoint, RecvStream, SendStream, crypto::rustls::QuicClientConfig};

pub const TOKEN_LEN: usize = 32;
pub const DEFAULT_AUTHORITY: &str = "/run/user/{uid}/.WinstreamAuthority-{port}";

#[derive(Clone)]
pub struct Auth {
    pub fingerprint: [u8; 32],
    pub token: [u8; TOKEN_LEN],
}

impl Auth {
    pub fn generate(cert_der: &[u8]) -> Result<Auth> {
        let mut token = [0u8; TOKEN_LEN];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut token)
            .map_err(|_| anyhow::anyhow!("no random numbers"))?;
        Ok(Auth {
            fingerprint: fingerprint(cert_der),
            token,
        })
    }

    pub fn write(&self, authority: &str, port: u16) -> Result<PathBuf> {
        let path = auth_path(authority, port)?;
        let dir = path.parent().context("the authority file has no folder")?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .with_context(|| format!("create {}", dir.display()))?;
        let mut tmp = path.clone().into_os_string();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)
            .with_context(|| format!("write {}", tmp.display()))?;
        file.write_all(self.encode().as_bytes())?;
        drop(file);
        std::fs::rename(&tmp, &path)?;
        Ok(path)
    }

    pub fn fetch(ssh: Option<&str>, authority: &str, port: u16) -> Result<Auth> {
        let text = match ssh {
            Some(dest) => {
                let out = Command::new("ssh")
                    .args(["-o", "BatchMode=yes", dest, "cat"])
                    .arg(fill(authority, "$(id -u)", port))
                    .output()
                    .context("run ssh")?;
                if !out.status.success() {
                    bail!(
                        "can't read the server's authority file over ssh {dest}: {} (is the server running there?)",
                        String::from_utf8_lossy(&out.stderr).trim()
                    );
                }
                String::from_utf8(out.stdout)?
            }
            None => {
                let path = auth_path(authority, port)?;
                std::fs::read_to_string(&path).with_context(|| {
                    format!(
                        "read {} (on another machine than the server, pass --ssh HOST)",
                        path.display()
                    )
                })?
            }
        };
        Auth::decode(&text).context("the authority file is malformed")
    }

    fn encode(&self) -> String {
        format!(
            "fingerprint {}\ntoken {}\n",
            hex(&self.fingerprint),
            hex(&self.token)
        )
    }

    fn decode(text: &str) -> Option<Auth> {
        let mut fingerprint = None;
        let mut token = None;
        for line in text.lines() {
            match line.split_once(' ')? {
                ("fingerprint", v) => fingerprint = Some(unhex::<32>(v)?),
                ("token", v) => token = Some(unhex::<TOKEN_LEN>(v)?),
                _ => {}
            }
        }
        Some(Auth {
            fingerprint: fingerprint?,
            token: token?,
        })
    }
}

pub fn token_matches(expected: &[u8; TOKEN_LEN], got: &[u8; TOKEN_LEN]) -> bool {
    expected
        .iter()
        .zip(got)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

pub fn transport() -> Result<quinn::TransportConfig> {
    let mut transport = quinn::TransportConfig::default();
    transport.max_idle_timeout(Some(crate::IDLE_TIMEOUT.try_into()?));
    transport.keep_alive_interval(Some(crate::KEEP_ALIVE));
    Ok(transport)
}

pub async fn connect(
    addr: SocketAddr,
    auth: &Auth,
    wait: Duration,
) -> Result<(Connection, SendStream, RecvStream)> {
    let mut endpoint = Endpoint::client(([0, 0, 0, 0], 0).into())?;
    endpoint.set_default_client_config(client_config(auth.fingerprint)?);
    let connecting = endpoint.connect(addr, "winstream")?;
    let conn = match tokio::time::timeout(wait, connecting).await {
        Ok(conn) => conn.context("connect")?,
        Err(_) => bail!(
            "no answer from {addr} within {} s: is the server running there?",
            wait.as_secs_f32()
        ),
    };
    let (mut send, recv) = conn.open_bi().await?;
    send.write_all(&auth.token).await?;
    Ok((conn, send, recv))
}

fn client_config(fingerprint: [u8; 32]) -> Result<quinn::ClientConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert {
            fingerprint,
            provider,
        }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![crate::ALPN.to_vec()];
    let mut config = quinn::ClientConfig::new(Arc::new(QuicClientConfig::try_from(tls)?));
    config.transport_config(Arc::new(transport()?));
    Ok(config)
}

pub fn auth_path(authority: &str, port: u16) -> Result<PathBuf> {
    let uid = std::fs::metadata("/proc/self")
        .context("find our user id")?
        .uid();
    let path = fill(authority, &uid.to_string(), port);
    Ok(match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var_os("HOME").context("HOME isn't set")?).join(rest),
        None => PathBuf::from(path),
    })
}

fn fill(authority: &str, uid: &str, port: u16) -> String {
    authority
        .replace("{uid}", uid)
        .replace("{port}", &port.to_string())
}

fn fingerprint(cert_der: &[u8]) -> [u8; 32] {
    let digest = ring::digest::digest(&ring::digest::SHA256, cert_der);
    let mut out = [0u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    let text = text.trim();
    if text.len() != N * 2 {
        return None;
    }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Debug)]
struct PinnedCert {
    fingerprint: [u8; 32],
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl rustls::client::danger::ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if fingerprint(end_entity) == self.fingerprint {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(
                "the server's certificate doesn't match its authority file (another server, or a stale file)".into(),
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_file_round_trips() {
        let auth = Auth::generate(b"cert").unwrap();
        let back = Auth::decode(&auth.encode()).unwrap();
        assert_eq!(back.fingerprint, auth.fingerprint);
        assert_eq!(back.token, auth.token);
    }

    #[test]
    fn malformed_auth_is_rejected() {
        assert!(Auth::decode("fingerprint 00\ntoken 11\n").is_none());
        assert!(Auth::decode("token ".to_string().as_str()).is_none());
    }

    #[test]
    fn tokens_compare() {
        let a = [7u8; TOKEN_LEN];
        let mut b = a;
        assert!(token_matches(&a, &b));
        b[31] = 8;
        assert!(!token_matches(&a, &b));
    }
}
