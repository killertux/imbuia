//! TLS transport for the optional remote-supervisor mode.
//!
//! The link between client and supervisor is normally a local Unix socket.
//! When the user configures a remote supervisor (`remote.url` in the global
//! config) and starts the supervisor with `--listen host:port`, the link is a
//! TCP connection wrapped in mutually-authenticated TLS.
//!
//! Trust is SSH-style, **not** CA/PKI:
//!  - Each side has a long-lived Ed25519 identity (`identity.key` in the config
//!    dir), presented at TLS time inside a throwaway self-signed cert. Only the
//!    public key matters — cert CN/SAN/validity are ignored.
//!  - A peer is identified by the **fingerprint** = `sha256(SubjectPublicKeyInfo)`,
//!    which is stable across cert regenerations (the cert is rebuilt every run,
//!    the key is not).
//!  - The client pins the supervisor's fingerprint **TOFU** in `known_hosts`.
//!  - The supervisor admits only client fingerprints listed in `authorized_keys`.

use crate::config;
use anyhow::{Context, Result, anyhow};
use std::fmt::Write as _;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    WebPkiSupportedAlgorithms, ring, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{
    ClientConfig, DigitallySignedStruct, DistinguishedName, Error as TlsError, ServerConfig,
    SignatureScheme,
};

/// Install the ring crypto provider as the process default. Idempotent; safe to
/// call from both client and supervisor entry points. We also pass the provider
/// explicitly to every config builder, but installing the default keeps any
/// internal rustls path that reaches for `CryptoProvider::get_default` happy.
pub fn init() {
    let _ = ring::default_provider().install_default();
}

/// A long-lived TLS identity: a self-signed cert over a persisted Ed25519 key,
/// plus this peer's own fingerprint (for logging / out-of-band exchange).
pub struct Identity {
    cert_der: Vec<u8>,
    key_pkcs8_der: Vec<u8>,
    pub fingerprint: String,
}

impl Identity {
    fn cert_chain(&self) -> Vec<CertificateDer<'static>> {
        vec![CertificateDer::from(self.cert_der.clone())]
    }
    fn key(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_pkcs8_der.clone()))
    }
}

/// Load the identity key from `<dir>/identity.key`, generating + persisting one
/// (mode 0600) on first use. The self-signed cert is rebuilt every call — only
/// the key is stable, and the fingerprint hashes the key, not the cert.
pub fn load_or_create_identity(dir: &Path) -> Result<Identity> {
    let path = config::identity_path(dir);
    let key_pair = if path.exists() {
        let pem = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        rcgen::KeyPair::from_pem(&pem).context("parsing identity.key")?
    } else {
        let kp = rcgen::KeyPair::generate_for(&rcgen::PKCS_ED25519)
            .context("generating identity key")?;
        write_private(&path, &kp.serialize_pem())?;
        kp
    };
    let params =
        rcgen::CertificateParams::new(vec!["imbuia".to_string()]).context("cert params")?;
    let cert = params.self_signed(&key_pair).context("self-signing cert")?;
    let cert_der = cert.der().to_vec();
    let key_pkcs8_der = key_pair.serialize_der();
    let fingerprint = fingerprint_of_cert(&cert_der)?;
    Ok(Identity {
        cert_der,
        key_pkcs8_der,
        fingerprint,
    })
}

/// `sha256(SubjectPublicKeyInfo)` of a DER cert, lowercase hex (SSH-style).
pub fn fingerprint_of_cert(cert_der: &[u8]) -> Result<String> {
    let (_, cert) = x509_parser::parse_x509_certificate(cert_der)
        .map_err(|e| anyhow!("parsing peer certificate: {e}"))?;
    Ok(hex_lower(&sha256(cert.tbs_certificate.subject_pki.raw)))
}

fn sha256(bytes: &[u8]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().into()
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    f.write_all(contents.as_bytes())
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Build the client TLS config: presents our identity cert (mutual auth) and
/// verifies the supervisor via the TOFU `known_hosts` pin.
pub fn client_config(id: &Identity, host: &str, dir: &Path) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(ring::default_provider());
    let algs = provider.signature_verification_algorithms;
    let verifier = Arc::new(TofuServerVerifier {
        host: host.to_string(),
        dir: dir.to_path_buf(),
        algs,
    });
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("tls protocol versions")?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(id.cert_chain(), id.key())
        .context("installing client identity cert")?;
    Ok(Arc::new(cfg))
}

/// Build the supervisor TLS config: presents our identity cert and admits only
/// clients whose fingerprint is in `authorized_keys`.
pub fn server_config(id: &Identity, dir: &Path) -> Result<Arc<ServerConfig>> {
    let provider = Arc::new(ring::default_provider());
    let algs = provider.signature_verification_algorithms;
    let verifier = Arc::new(AuthorizedKeysVerifier {
        dir: dir.to_path_buf(),
        algs,
    });
    let cfg = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("tls protocol versions")?
        .with_client_cert_verifier(verifier)
        .with_single_cert(id.cert_chain(), id.key())
        .context("installing supervisor identity cert")?;
    Ok(Arc::new(cfg))
}

/// Build a `ServerName` for the TLS handshake from a bare host (no port). The
/// verifier ignores it, but rustls still requires a syntactically valid name.
pub fn server_name(host: &str) -> Result<ServerName<'static>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        Ok(ServerName::IpAddress(ip.into()))
    } else {
        ServerName::try_from(host.to_string()).map_err(|_| anyhow!("invalid server name {host:?}"))
    }
}

/// Split a `host:port` url into `(host, port_str)`. IPv6 literals must be
/// bracketed (`[::1]:7777`).
pub fn split_host_port(url: &str) -> Result<(&str, &str)> {
    if let Some(rest) = url.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("malformed bracketed host in {url:?}"))?;
        let port = tail
            .strip_prefix(':')
            .ok_or_else(|| anyhow!("missing port in {url:?}"))?;
        return Ok((host, port));
    }
    url.rsplit_once(':')
        .ok_or_else(|| anyhow!("expected host:port, got {url:?}"))
}

// --- Pinned-key verifiers -------------------------------------------------

/// Client-side: TOFU pin of the supervisor's public key in `known_hosts`.
#[derive(Debug)]
struct TofuServerVerifier {
    host: String,
    dir: PathBuf,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for TofuServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let fp = fingerprint_of_cert(end_entity)
            .map_err(|e| TlsError::General(format!("fingerprint: {e}")))?;
        match config::known_host_fingerprint(&self.dir, &self.host) {
            Some(known) if known == fp => Ok(ServerCertVerified::assertion()),
            Some(known) => Err(TlsError::General(format!(
                "supervisor key for {} changed (known {known}, got {fp}); refusing — \
                 remove the line from known_hosts to re-trust",
                self.host
            ))),
            None => {
                config::append_known_host(&self.dir, &self.host, &fp)
                    .map_err(|e| TlsError::General(format!("persisting known_host: {e}")))?;
                tracing::warn!(host = %self.host, fingerprint = %fp, "TOFU: pinned new supervisor key");
                Ok(ServerCertVerified::assertion())
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

/// Supervisor-side: admit only client fingerprints in `authorized_keys`.
#[derive(Debug)]
struct AuthorizedKeysVerifier {
    dir: PathBuf,
    algs: WebPkiSupportedAlgorithms,
}

impl ClientCertVerifier for AuthorizedKeysVerifier {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, TlsError> {
        let fp = fingerprint_of_cert(end_entity)
            .map_err(|e| TlsError::General(format!("fingerprint: {e}")))?;
        if config::load_authorized_fingerprints(&self.dir).contains(&fp) {
            tracing::info!(fingerprint = %fp, "client authorized");
            Ok(ClientCertVerified::assertion())
        } else {
            tracing::warn!(fingerprint = %fp, "client rejected: not in authorized_keys");
            Err(TlsError::General(format!(
                "client key {fp} not in authorized_keys"
            )))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algs.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_across_cert_regen_same_key() {
        let dir = std::env::temp_dir().join(format!("imbuia-tp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // Two identities loaded from the SAME persisted key (cert rebuilt each
        // time) must share a fingerprint — that's what makes TOFU work.
        let a = load_or_create_identity(&dir).unwrap();
        let b = load_or_create_identity(&dir).unwrap();
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_eq!(a.fingerprint.len(), 64); // sha256 hex
        assert!(a.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn distinct_keys_have_distinct_fingerprints() {
        let d1 = std::env::temp_dir().join(format!("imbuia-tp1-{}", std::process::id()));
        let d2 = std::env::temp_dir().join(format!("imbuia-tp2-{}", std::process::id()));
        for d in [&d1, &d2] {
            let _ = std::fs::remove_dir_all(d);
            std::fs::create_dir_all(d).unwrap();
        }
        let a = load_or_create_identity(&d1).unwrap();
        let b = load_or_create_identity(&d2).unwrap();
        assert_ne!(a.fingerprint, b.fingerprint);
        for d in [&d1, &d2] {
            std::fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn split_host_port_forms() {
        assert_eq!(split_host_port("h:1").unwrap(), ("h", "1"));
        assert_eq!(
            split_host_port("example.com:7777").unwrap(),
            ("example.com", "7777")
        );
        assert_eq!(split_host_port("[::1]:22").unwrap(), ("::1", "22"));
        assert!(split_host_port("noport").is_err());
    }

    // --- loopback mutual-TLS handshake tests -----------------------------

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "imbuia-tls-{tag}-{}-{:p}",
            std::process::id(),
            &tag as *const _
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Stand up a loopback TLS server with `server_config`, returning its addr
    /// and a task that accepts one connection and echoes one framed `u8`.
    async fn echo_server(
        cfg: Arc<ServerConfig>,
    ) -> (std::net::SocketAddr, tokio::task::JoinHandle<bool>) {
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(cfg);
        let handle = tokio::spawn(async move {
            let Ok((tcp, _)) = listener.accept().await else {
                return false;
            };
            // Returns true iff the (mutually-authenticated) handshake succeeds.
            match acceptor.accept(tcp).await {
                Ok(mut tls) => {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut b = [0u8; 1];
                    if tls.read_exact(&mut b).await.is_ok() {
                        let _ = tls.write_all(&b).await;
                        let _ = tls.flush().await;
                    }
                    true
                }
                Err(_) => false,
            }
        });
        (addr, handle)
    }

    #[tokio::test]
    async fn mutual_tls_happy_path_tofu_and_authorized() {
        init();
        let cdir = tmpdir("c-ok");
        let sdir = tmpdir("s-ok");
        let cid = load_or_create_identity(&cdir).unwrap();
        let sid = load_or_create_identity(&sdir).unwrap();
        // Authorize the client on the server.
        std::fs::write(
            config::authorized_keys_path(&sdir),
            format!("{}\n", cid.fingerprint),
        )
        .unwrap();

        let (addr, server) = echo_server(server_config(&sid, &sdir).unwrap()).await;

        let connector =
            tokio_rustls::TlsConnector::from(client_config(&cid, "localhost", &cdir).unwrap());
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let mut tls = connector
            .connect(server_name("localhost").unwrap(), tcp)
            .await
            .expect("client handshake should succeed");

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        tls.write_all(&[42]).await.unwrap();
        tls.flush().await.unwrap();
        let mut b = [0u8; 1];
        tls.read_exact(&mut b).await.unwrap();
        assert_eq!(b[0], 42);
        assert!(server.await.unwrap());

        // TOFU stored the server's key under the host on first connect.
        assert_eq!(
            config::known_host_fingerprint(&cdir, "localhost").as_deref(),
            Some(sid.fingerprint.as_str())
        );

        std::fs::remove_dir_all(&cdir).unwrap();
        std::fs::remove_dir_all(&sdir).unwrap();
    }

    #[tokio::test]
    async fn unauthorized_client_is_rejected() {
        init();
        let cdir = tmpdir("c-no");
        let sdir = tmpdir("s-no");
        let cid = load_or_create_identity(&cdir).unwrap();
        let sid = load_or_create_identity(&sdir).unwrap();
        // Intentionally do NOT add cid to the server's authorized_keys.

        let (addr, server) = echo_server(server_config(&sid, &sdir).unwrap()).await;

        let connector =
            tokio_rustls::TlsConnector::from(client_config(&cid, "localhost", &cdir).unwrap());
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        // In TLS 1.3 the client's `connect()` can resolve before the server
        // validates the client cert, so the rejection surfaces either here or
        // on the first I/O. Drive a round-trip and assert it can't complete.
        let echoed = match connector
            .connect(server_name("localhost").unwrap(), tcp)
            .await
        {
            Err(_) => false,
            Ok(mut tls) => {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut b = [0u8; 1];
                tls.write_all(&[7]).await.is_ok()
                    && tls.flush().await.is_ok()
                    && tls.read_exact(&mut b).await.is_ok()
            }
        };
        assert!(
            !echoed,
            "unauthorized client must not complete a round-trip"
        );
        assert!(!server.await.unwrap(), "server must reject the handshake");

        std::fs::remove_dir_all(&cdir).unwrap();
        std::fs::remove_dir_all(&sdir).unwrap();
    }

    #[tokio::test]
    async fn tofu_rejects_changed_server_key() {
        init();
        let cdir = tmpdir("c-mm");
        let sdir = tmpdir("s-mm");
        let cid = load_or_create_identity(&cdir).unwrap();
        let sid = load_or_create_identity(&sdir).unwrap();
        std::fs::write(
            config::authorized_keys_path(&sdir),
            format!("{}\n", cid.fingerprint),
        )
        .unwrap();
        // Pre-pin a DIFFERENT key for the host: a later real connect must fail.
        config::append_known_host(&cdir, "localhost", &"a".repeat(64)).unwrap();

        let (addr, server) = echo_server(server_config(&sid, &sdir).unwrap()).await;
        let connector =
            tokio_rustls::TlsConnector::from(client_config(&cid, "localhost", &cdir).unwrap());
        let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
        let res = connector
            .connect(server_name("localhost").unwrap(), tcp)
            .await;
        assert!(res.is_err(), "client must reject a changed server key");
        let _ = server.await;

        std::fs::remove_dir_all(&cdir).unwrap();
        std::fs::remove_dir_all(&sdir).unwrap();
    }
}
