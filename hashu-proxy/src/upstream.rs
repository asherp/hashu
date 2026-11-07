//! Dialing upstream pools, over plain TCP or TLS.
//!
//! An upstream is kept as a string so routes stay cheap to clone and easy
//! to log: `host:port` for plain TCP, `tls://host:port` for TLS.
//! [`normalize_upstream`] turns a user-facing pool URL
//! (`stratum+tcp://…`, `stratum+ssl://…`) into that form, and [`dial`]
//! opens it.
//!
//! TLS verifies the pool's certificate against the Mozilla roots bundled
//! by `webpki-roots`, with the URL's host as the server name. A pool with a
//! self-signed or expired certificate fails the handshake, which the
//! router treats like an unreachable pool (fallback to the default pool).

use std::io;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rustls::pki_types::ServerName;
use rustls::ClientConfig;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

/// Prefix [`normalize_upstream`] puts on a TLS upstream.
pub const TLS_PREFIX: &str = "tls://";

/// How long [`dial`] waits for the TCP connect plus TLS handshake.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

/// A connected pool socket: plain TCP or TLS over TCP.
pub trait PoolIo: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> PoolIo for T {}

/// Boxed pool socket returned by [`dial`].
pub type PoolStream = Box<dyn PoolIo>;

/// Turn a pool URL into a dialable upstream: `host:port` for
/// `stratum+tcp://`, `stratum://` or a bare `host:port`, and
/// `tls://host:port` for `stratum+ssl://`, `stratum+tcps://` or `tls://`.
/// `None` if there is no port, the port is not a number, or there is a
/// path (an SV2 URL such as `stratum2+tcp://host:port/key` is refused too,
/// since its scheme is not one of the above).
pub fn normalize_upstream(url: &str) -> Option<String> {
    let url = url.trim();
    let (tls, rest) = if let Some(r) = url
        .strip_prefix("stratum+ssl://")
        .or_else(|| url.strip_prefix("stratum+tcps://"))
        .or_else(|| url.strip_prefix(TLS_PREFIX))
    {
        (true, r)
    } else if let Some(r) = url
        .strip_prefix("stratum+tcp://")
        .or_else(|| url.strip_prefix("stratum://"))
    {
        (false, r)
    } else if url.contains("://") {
        return None;
    } else {
        (false, url)
    };
    if rest.contains('/') {
        return None;
    }
    let (host, port) = rest.rsplit_once(':')?;
    if host.is_empty() || port.parse::<u16>().ok()? == 0 {
        return None;
    }
    Some(if tls {
        format!("{TLS_PREFIX}{rest}")
    } else {
        rest.to_string()
    })
}

/// Whether `upstream` (as produced by [`normalize_upstream`]) is TLS.
pub fn is_tls(upstream: &str) -> bool {
    upstream.starts_with(TLS_PREFIX)
}

/// Connect to `upstream`, doing the TLS handshake for a `tls://` one.
/// Fails after [`DIAL_TIMEOUT`].
pub async fn dial(upstream: &str) -> io::Result<PoolStream> {
    dial_with(upstream, default_tls_config()).await
}

/// Like [`dial`], with an explicit TLS client config (tests trust a local
/// CA this way).
pub async fn dial_with(upstream: &str, tls: Arc<ClientConfig>) -> io::Result<PoolStream> {
    tokio::time::timeout(DIAL_TIMEOUT, dial_inner(upstream, tls))
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("dial {upstream}: timed out after {DIAL_TIMEOUT:?}"),
            )
        })?
}

async fn dial_inner(upstream: &str, tls: Arc<ClientConfig>) -> io::Result<PoolStream> {
    let Some(addr) = upstream.strip_prefix(TLS_PREFIX) else {
        let tcp = TcpStream::connect(upstream).await?;
        tcp.set_nodelay(true).ok();
        return Ok(Box::new(tcp));
    };
    let host = host_of(addr);
    let name = ServerName::try_from(host.to_string()).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("bad TLS server name {host:?}: {e}"),
        )
    })?;
    let tcp = TcpStream::connect(addr).await?;
    tcp.set_nodelay(true).ok();
    let stream = TlsConnector::from(tls).connect(name, tcp).await?;
    Ok(Box::new(stream))
}

/// Host part of `host:port`, without brackets for an IPv6 literal.
fn host_of(addr: &str) -> &str {
    let host = addr.rsplit_once(':').map_or(addr, |(h, _)| h);
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// TLS client config trusting the `webpki-roots` Mozilla bundle. Built once.
pub fn default_tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let roots =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(client_config(roots))
        })
        .clone()
}

/// TLS 1.2 or 1.3 client config with the `ring` provider and `roots`.
/// Pinning the provider avoids depending on a process-wide default, which
/// rustls refuses to guess when more than one is compiled in.
pub fn client_config(roots: rustls::RootCertStore) -> ClientConfig {
    ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .with_root_certificates(roots)
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    const CA: &[u8] = include_bytes!("../tests/fixtures/test-ca.pem");
    const CERT: &[u8] = include_bytes!("../tests/fixtures/pool-cert.pem");
    const KEY: &[u8] = include_bytes!("../tests/fixtures/pool-key.pem");

    #[test]
    fn normalizes_pool_urls() {
        let n = |s| normalize_upstream(s);
        assert_eq!(
            n("stratum+tcp://p.example:3333").as_deref(),
            Some("p.example:3333")
        );
        assert_eq!(
            n("stratum://p.example:3333").as_deref(),
            Some("p.example:3333")
        );
        assert_eq!(n("p.example:3333").as_deref(), Some("p.example:3333"));
        assert_eq!(
            n("stratum+ssl://btcssl.f2pool.com:1300").as_deref(),
            Some("tls://btcssl.f2pool.com:1300")
        );
        assert_eq!(n("stratum+tcps://p:443").as_deref(), Some("tls://p:443"));
        assert_eq!(n("tls://p:443").as_deref(), Some("tls://p:443"));
        // Refused: no port, bad port, path, other schemes.
        assert_eq!(n("stratum+tcp://noport"), None);
        assert_eq!(n("stratum+ssl://noport"), None);
        assert_eq!(n("stratum+tcp://p:0"), None);
        assert_eq!(n("stratum+tcp://p:x"), None);
        assert_eq!(n("stratum+tcp://p:3333/key"), None);
        assert_eq!(n("stratum2+tcp://stratum.braiins.com:3333/abc"), None);
        assert_eq!(n("https://p:443"), None);
        assert_eq!(n(":3333"), None);
    }

    #[test]
    fn host_of_strips_port_and_brackets() {
        assert_eq!(host_of("pool.example:1300"), "pool.example");
        assert_eq!(host_of("[::1]:1300"), "::1");
        assert_eq!(host_of("127.0.0.1:1300"), "127.0.0.1");
    }

    fn test_ca_config() -> Arc<ClientConfig> {
        let mut roots = rustls::RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(CA) {
            roots.add(c.unwrap()).unwrap();
        }
        Arc::new(client_config(roots))
    }

    /// TLS echo server on 127.0.0.1: reads one line, writes it back.
    async fn tls_echo_pool() -> std::net::SocketAddr {
        let certs: Vec<_> = CertificateDer::pem_slice_iter(CERT)
            .map(|c| c.unwrap())
            .collect();
        let key = PrivateKeyDer::from_pem_slice(KEY).unwrap();
        let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(cfg));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((sock, _)) = listener.accept().await {
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    let Ok(tls) = acceptor.accept(sock).await else {
                        return;
                    };
                    let (r, mut w) = tokio::io::split(tls);
                    let mut line = String::new();
                    BufReader::new(r).read_line(&mut line).await.unwrap();
                    w.write_all(line.as_bytes()).await.unwrap();
                    w.flush().await.unwrap();
                });
            }
        });
        addr
    }

    #[tokio::test]
    async fn dials_tls_pool_with_trusted_cert() {
        let addr = tls_echo_pool().await;
        let upstream = format!("tls://localhost:{}", addr.port());
        let s = dial_with(&upstream, test_ca_config()).await.unwrap();
        let (r, mut w) = tokio::io::split(s);
        w.write_all(b"{\"id\":1}\n").await.unwrap();
        let mut line = String::new();
        BufReader::new(r).read_line(&mut line).await.unwrap();
        assert_eq!(line, "{\"id\":1}\n");
    }

    #[tokio::test]
    async fn refuses_untrusted_cert() {
        let addr = tls_echo_pool().await;
        let upstream = format!("tls://localhost:{}", addr.port());
        // The default config trusts only public roots, not the test CA.
        assert!(dial(&upstream).await.is_err());
    }

    #[tokio::test]
    async fn dials_tls_pool_by_ip() {
        let addr = tls_echo_pool().await;
        // The test cert has an IP SAN for 127.0.0.1.
        let upstream = format!("tls://127.0.0.1:{}", addr.port());
        assert!(dial_with(&upstream, test_ca_config()).await.is_ok());
    }
}
