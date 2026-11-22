//! The public Stratum address miners connect to.
//!
//! The proxy binds `--stratum-bind` (under compose, the container's
//! `0.0.0.0:3333`), but miners reach it through whatever host name and
//! port mapping the deployment puts in front of it (`mint.hashu.io:3335`).
//! The mint cannot see that mapping, so the operator names it with
//! `HASHU_STRATUM_URL`; without it the address is derived from the mint's
//! public URL host and the bind port, which is right only when nothing
//! remaps the port.
//!
//! The same value is shown everywhere a rig owner is told where to point
//! a miner: `/checkout/api/info`, the operator dashboard, the supplier
//! join response and the waitlist page.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use serde::Serialize;

/// Schemes a miner may be pointed at.
const SCHEMES: [&str; 3] = ["stratum+tcp", "stratum+ssl", "stratum2+tcp"];

/// Where miners connect: `url` as typed into most miners, and its `host`
/// and `port` for firmware with separate fields (AxeOS).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StratumAddr {
    pub url: String,
    pub host: String,
    pub port: u16,
}

impl StratumAddr {
    /// Parse `HASHU_STRATUM_URL`: `stratum+tcp://host:port` (also
    /// `stratum+ssl://`, `stratum2+tcp://`), or a bare `host:port`, taken
    /// as `stratum+tcp`. The port is required: there is no standard one.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim().trim_end_matches('/');
        let full = if raw.contains("://") {
            raw.to_string()
        } else {
            format!("stratum+tcp://{raw}")
        };
        let uri: axum::http::Uri = full
            .parse()
            .with_context(|| format!("HASHU_STRATUM_URL is not a URL: {raw}"))?;
        let scheme = uri.scheme_str().unwrap_or_default().to_ascii_lowercase();
        if !SCHEMES.contains(&scheme.as_str()) {
            bail!("HASHU_STRATUM_URL must start with stratum+tcp://, stratum+ssl:// or stratum2+tcp:// (got {raw})");
        }
        let Some(host) = uri.host().filter(|h| !h.is_empty()) else {
            bail!("HASHU_STRATUM_URL has no host: {raw}");
        };
        let Some(port) = uri.port_u16() else {
            bail!("HASHU_STRATUM_URL needs a port, e.g. stratum+tcp://{host}:3333 (got {raw})");
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        Ok(Self::new(&scheme, &host, port))
    }

    /// The address implied when `HASHU_STRATUM_URL` is unset: the mint's
    /// public URL host (else the bind address, loopback for `0.0.0.0`) and
    /// the bind port.
    pub fn derive(mint_url: Option<&str>, bind: SocketAddr) -> Self {
        let host = mint_url
            .and_then(|u| u.trim().parse::<axum::http::Uri>().ok())
            .and_then(|u| {
                u.host()
                    .map(|h| h.trim_end_matches('.').to_ascii_lowercase())
            })
            .filter(|h| !h.is_empty())
            .unwrap_or_else(|| {
                let ip = if bind.ip().is_unspecified() {
                    "127.0.0.1".to_string()
                } else {
                    bind.ip().to_string()
                };
                if ip.contains(':') {
                    format!("[{ip}]")
                } else {
                    ip
                }
            });
        Self::new("stratum+tcp", &host, bind.port())
    }

    fn new(scheme: &str, host: &str, port: u16) -> Self {
        Self {
            url: format!("{scheme}://{host}:{port}"),
            host: host.to_string(),
            port,
        }
    }

    /// `host:port`, for display.
    pub fn host_port(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// The public Stratum address from `--stratum-url`, or derived (see
/// [`StratumAddr::derive`]) with a warning when the mint has a public URL
/// and the derived port is only a guess.
pub fn resolve(
    stratum_url: Option<&str>,
    mint_url: Option<&str>,
    bind: SocketAddr,
) -> Result<StratumAddr> {
    if let Some(raw) = stratum_url.filter(|u| !u.trim().is_empty()) {
        return StratumAddr::parse(raw);
    }
    let addr = StratumAddr::derive(mint_url, bind);
    if mint_url.is_some_and(|u| !u.trim().is_empty()) {
        tracing::warn!(
            stratum = %addr.url,
            "HASHU_STRATUM_URL is not set: telling miners to connect to {}, the mint's host and the \
             bind port; set HASHU_STRATUM_URL if a port mapping or another host is in front of the proxy",
            addr.host_port()
        );
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urls_and_bare_host_ports() {
        let want = StratumAddr {
            url: "stratum+tcp://mint.example.com:3335".into(),
            host: "mint.example.com".into(),
            port: 3335,
        };
        for raw in [
            "stratum+tcp://mint.example.com:3335",
            " stratum+tcp://Mint.Example.com.:3335/ ",
            "mint.example.com:3335",
        ] {
            assert_eq!(StratumAddr::parse(raw).unwrap(), want, "{raw}");
        }
        let ssl = StratumAddr::parse("stratum+ssl://mint.example.com:4443").unwrap();
        assert_eq!(ssl.url, "stratum+ssl://mint.example.com:4443");
    }

    #[test]
    fn refuses_bad_urls() {
        for raw in [
            "http://mint.example.com:3335",
            "stratum+tcp://mint.example.com",
            "mint.example.com",
            "stratum+tcp://:3335",
        ] {
            assert!(StratumAddr::parse(raw).is_err(), "{raw}");
        }
    }

    #[test]
    fn derives_from_mint_host_and_bind_port() {
        let bind: SocketAddr = "0.0.0.0:3333".parse().unwrap();
        let a = StratumAddr::derive(Some("https://mint.example.com"), bind);
        assert_eq!(a.url, "stratum+tcp://mint.example.com:3333");
        let local = StratumAddr::derive(None, bind);
        assert_eq!(local.url, "stratum+tcp://127.0.0.1:3333");
        let v6 = StratumAddr::derive(None, "[::1]:3333".parse().unwrap());
        assert_eq!(v6.host_port(), "[::1]:3333");
    }

    #[test]
    fn the_env_value_wins_over_the_derived_one() {
        let bind: SocketAddr = "0.0.0.0:3333".parse().unwrap();
        let a = resolve(
            Some("stratum+tcp://mint.example.com:3335"),
            Some("https://mint.example.com"),
            bind,
        )
        .unwrap();
        assert_eq!(a.port, 3335);
        let d = resolve(Some(""), Some("https://mint.example.com"), bind).unwrap();
        assert_eq!(d.port, 3333);
    }
}
