//! `hashu wallet` — fresh BTC wallet utility for testing.
//!
//! `hashu wallet generate` produces a one-shot mainnet P2WPKH (`bc1q…`)
//! address + WIF privkey + pubkey hex on stdout. Output is *not*
//! persisted — paste into your password manager if you want to keep it.
//!
//! Intended for the live `hashu mine` ⇄ `hashu proxy` ⇄ public pool test
//! flow: solo pools want a real BTC address as the worker username, but
//! a CPU has no realistic chance of finding a block, so a throwaway key
//! is fine.

use anyhow::{Context, Result};
use bitcoin::secp256k1::{rand, Secp256k1, SecretKey};
use bitcoin::{Address, CompressedPublicKey, Network, NetworkKind, PrivateKey};
use clap::{Args, Subcommand};

#[derive(Debug, Args)]
pub struct WalletCmd {
    #[command(subcommand)]
    pub action: WalletAction,
}

#[derive(Debug, Subcommand)]
pub enum WalletAction {
    /// Print a fresh BTC P2WPKH (`bc1q…`) address + WIF + compressed pubkey.
    Generate(GenerateArgs),
}

#[derive(Debug, Args)]
pub struct GenerateArgs {
    /// Network for the address. `bitcoin` (default), `testnet`, `signet`,
    /// or `regtest`.
    #[arg(long, default_value = "bitcoin")]
    pub network: String,
}

pub async fn run(cmd: WalletCmd) -> Result<()> {
    match cmd.action {
        WalletAction::Generate(args) => generate(args),
    }
}

fn generate(args: GenerateArgs) -> Result<()> {
    let network = parse_network(&args.network)?;
    let secp = Secp256k1::new();
    let sk = SecretKey::new(&mut rand::thread_rng());
    let priv_key = PrivateKey::new(sk, NetworkKind::from(network));
    let pub_key = CompressedPublicKey::from_private_key(&secp, &priv_key)
        .context("derive compressed pubkey")?;
    let address = Address::p2wpkh(&pub_key, network);

    println!("network: {network}");
    println!("address: {address}");
    println!("wif:     {}", priv_key.to_wif());
    println!("pubkey:  {}", hex::encode(pub_key.to_bytes()));
    Ok(())
}

fn parse_network(s: &str) -> Result<Network> {
    match s.to_ascii_lowercase().as_str() {
        "bitcoin" | "mainnet" => Ok(Network::Bitcoin),
        "testnet" => Ok(Network::Testnet),
        "signet" => Ok(Network::Signet),
        "regtest" => Ok(Network::Regtest),
        other => anyhow::bail!(
            "unknown network {other:?}; expected bitcoin / testnet / signet / regtest",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Address;
    use std::str::FromStr;

    #[test]
    fn parse_network_accepts_aliases() {
        assert_eq!(parse_network("bitcoin").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("MAINNET").unwrap(), Network::Bitcoin);
        assert_eq!(parse_network("testnet").unwrap(), Network::Testnet);
        assert_eq!(parse_network("signet").unwrap(), Network::Signet);
        assert_eq!(parse_network("regtest").unwrap(), Network::Regtest);
        assert!(parse_network("nonsense").is_err());
    }

    #[test]
    fn generated_address_round_trips_for_each_network() {
        let secp = Secp256k1::new();
        for net in [
            Network::Bitcoin,
            Network::Testnet,
            Network::Signet,
            Network::Regtest,
        ] {
            let sk = SecretKey::new(&mut rand::thread_rng());
            let priv_key = PrivateKey::new(sk, NetworkKind::from(net));
            let pub_key = CompressedPublicKey::from_private_key(&secp, &priv_key).unwrap();
            let address = Address::p2wpkh(&pub_key, net);
            let addr_str = address.to_string();
            let parsed = Address::from_str(&addr_str)
                .unwrap()
                .require_network(net)
                .unwrap_or_else(|_| panic!("network mismatch on {addr_str}"));
            assert_eq!(parsed.to_string(), addr_str);
        }
    }

    #[test]
    fn mainnet_addresses_have_bc1q_prefix() {
        let secp = Secp256k1::new();
        for _ in 0..16 {
            let sk = SecretKey::new(&mut rand::thread_rng());
            let priv_key = PrivateKey::new(sk, NetworkKind::Main);
            let pub_key = CompressedPublicKey::from_private_key(&secp, &priv_key).unwrap();
            let address = Address::p2wpkh(&pub_key, Network::Bitcoin).to_string();
            assert!(
                address.starts_with("bc1q"),
                "expected bc1q prefix, got {address}"
            );
        }
    }
}
