//! `hashu manifest` subcommands — apply observed keyset, build kind 0,
//! publish or dry-run.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hashu_mint::manifest::{
    self, FixedKeysetSource, KeysetSource, ManifestState, Transition,
};
use nostr::{JsonUtil, ToBech32};

use crate::init::{load_keys, resolve_data_dir, state_path};

#[derive(Debug, Args)]
pub struct ManifestCmd {
    #[command(subcommand)]
    pub action: ManifestAction,
}

#[derive(Debug, Subcommand)]
pub enum ManifestAction {
    /// Build a kind 0 event and publish it to the configured relays.
    Publish(ActionArgs),
    /// Build the kind 0 event and print it without publishing.
    /// Does not mutate state on disk.
    DryRun(ActionArgs),
}

#[derive(Debug, Args)]
pub struct ActionArgs {
    /// Active mint keyset id (hex). Required until cdk integration lands.
    #[arg(long)]
    pub keyset_id: String,
    /// Override the data directory.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

pub async fn run(cmd: ManifestCmd) -> Result<()> {
    match cmd.action {
        ManifestAction::Publish(args) => run_publish(args).await,
        ManifestAction::DryRun(args) => run_dry_run(args).await,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn run_publish(args: ActionArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir)?;
    let state_p = state_path(&data_dir);
    let mut state = ManifestState::load_from_path(&state_p)
        .with_context(|| format!("load {}", state_p.display()))?;
    let keys = load_keys(&data_dir)?;

    let source = FixedKeysetSource::new(args.keyset_id);
    let observed = source.current_active()?;
    let now = now_secs();
    let transition = state.apply_observed_keyset(&observed, now)?;
    log_transition(transition, &observed);

    let event = manifest::build_event(&state, &keys, now)?;
    let outcome = manifest::publish(&event, &state.relays).await?;

    // Persist the latest issued_at so future events stay strictly later.
    state.last_published_at = Some(event.created_at.as_secs());
    state
        .save_atomic_to_path(&state_p)
        .with_context(|| format!("save {}", state_p.display()))?;

    println!("event_id:  {}", outcome.event_id);
    println!("nevent:    {}", event.id.to_bech32().unwrap_or_default());
    println!("created_at: {}", event.created_at.as_secs());
    println!("succeeded: {}", outcome.succeeded.len());
    for u in &outcome.succeeded {
        println!("  - {u}");
    }
    if !outcome.failed.is_empty() {
        println!("failed:    {}", outcome.failed.len());
        for (u, e) in &outcome.failed {
            println!("  - {u}: {e}");
        }
    }
    Ok(())
}

async fn run_dry_run(args: ActionArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir)?;
    let state_p = state_path(&data_dir);
    // Clone-then-mutate: dry-run never writes to disk.
    let mut state = ManifestState::load_from_path(&state_p)
        .with_context(|| format!("load {}", state_p.display()))?;
    let keys = load_keys(&data_dir)?;

    let source = FixedKeysetSource::new(args.keyset_id);
    let observed = source.current_active()?;
    let now = now_secs();
    let transition = state.apply_observed_keyset(&observed, now)?;
    log_transition(transition, &observed);

    let event = manifest::build_event(&state, &keys, now)?;
    let nevent = event.id.to_bech32().unwrap_or_default();
    let json = event.as_pretty_json();

    println!("(dry-run; no state written, no relay contact)");
    println!("nevent: {nevent}");
    println!();
    println!("{json}");
    Ok(())
}

fn log_transition(t: Transition, observed: &str) {
    match t {
        Transition::Initial => eprintln!("transition: Initial (first keyset {observed})"),
        Transition::NoOp => eprintln!("transition: NoOp ({observed} already active)"),
        Transition::Rotated {} => {
            eprintln!("transition: Rotated (new active {observed}; prior marked retired)")
        }
    }
}
