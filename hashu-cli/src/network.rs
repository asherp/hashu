//! Bitcoin network selection shared by CLI subcommands.
//!
//! Accepts `bitcoin` (alias `mainnet`), `testnet4`, `testnet` (Testnet3,
//! alias `testnet3`), `signet` and `regtest`. Testnet4 (BIP 94) is the
//! network the prototype demo runs on.

use anyhow::Result;
use bitcoin::Network;
use hashu_mint::oracle::esplora;

/// Names accepted by [`parse_network`], for help/error text.
pub const NETWORK_NAMES: &str = "bitcoin / testnet4 / testnet / signet / regtest";

/// Parse a user-supplied network name (case-insensitive).
pub fn parse_network(s: &str) -> Result<Network> {
    match s.to_ascii_lowercase().as_str() {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet4" => Ok(Network::Testnet4),
        "testnet" | "testnet3" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        other => anyhow::bail!("unknown network {other:?}; expected {NETWORK_NAMES}"),
    }
}

/// Default public Esplora base URL for `network`, or `None` when there is no
/// public instance (regtest), in which case the caller must pass a URL.
pub fn default_esplora_url(network: Network) -> Option<&'static str> {
    match network {
        Network::Bitcoin => Some(esplora::DEFAULT_BASE_URL),
        Network::Testnet4 => Some(esplora::TESTNET4_BASE_URL),
        Network::Testnet => Some(esplora::TESTNET3_BASE_URL),
        Network::Signet => Some(esplora::SIGNET_BASE_URL),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_network_accepts_aliases() {
        assert_eq!(parse_network("bitcoin").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("MAINNET").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("testnet4").unwrap(), Network::Testnet4);
        assert_eq!(parse_network("Testnet4").unwrap(), Network::Testnet4);
        assert_eq!(parse_network("testnet").unwrap(), Network::Testnet);
        assert_eq!(parse_network("testnet3").unwrap(), Network::Testnet);
        assert_eq!(parse_network("signet").unwrap(), Network::Signet);
        assert_eq!(parse_network("regtest").unwrap(), Network::Regtest);
        assert!(parse_network("nonsense").is_err());
    }

    #[test]
    fn esplora_urls_per_network() {
        assert_eq!(
            default_esplora_url(Network::Bitcoin),
            Some("https://blockstream.info/api")
        );
        assert_eq!(
            default_esplora_url(Network::Testnet4),
            Some("https://mempool.space/testnet4/api")
        );
        assert_eq!(
            default_esplora_url(Network::Testnet),
            Some("https://blockstream.info/testnet/api")
        );
        assert_eq!(
            default_esplora_url(Network::Signet),
            Some("https://mempool.space/signet/api")
        );
        assert_eq!(default_esplora_url(Network::Regtest), None);
    }
}
