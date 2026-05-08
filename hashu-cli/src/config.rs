//! `hashu config` — inspect and update the operator state file.
//!
//! Shape mirrors `oracle` and `manifest`: read-only `show`, atomic-write
//! `set` with named flags. Unspecified flags leave fields alone; passing an
//! empty string to a profile flag clears it.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use hashu_mint::manifest::ManifestState;
use hashu_mint::oracle::{esplora, luxor};

use crate::init::{resolve_data_dir, state_path};

#[derive(Debug, Args)]
pub struct ConfigCmd {
    #[command(subcommand)]
    pub action: ConfigAction,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the current operator state as JSON.
    Show(ShowArgs),
    /// Update one or more fields. Unspecified flags are left alone; pass
    /// an empty string ("") to clear an optional profile field.
    Set(Box<SetArgs>),
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct SetArgs {
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    #[arg(long)]
    pub instance_url: Option<String>,
    /// `esplora` or `luxor`. If you change this without --oracle-url, the
    /// URL is reset to the new source's compiled-in default.
    #[arg(long)]
    pub oracle_source: Option<String>,
    #[arg(long)]
    pub oracle_url: Option<String>,
    /// Comma-separated. Replaces the entire relay list.
    #[arg(long)]
    pub relays: Option<String>,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub about: Option<String>,
    #[arg(long)]
    pub nip05: Option<String>,
    #[arg(long)]
    pub lud16: Option<String>,
    #[arg(long)]
    pub website: Option<String>,
}

pub async fn run(cmd: ConfigCmd) -> Result<()> {
    match cmd.action {
        ConfigAction::Show(args) => run_show(args),
        ConfigAction::Set(args) => run_set(*args),
    }
}

fn run_show(args: ShowArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir)?;
    let path = state_path(&data_dir);
    let state = ManifestState::load_from_path(&path)
        .with_context(|| format!("load {}", path.display()))?;
    let json = serde_json::to_string_pretty(&state)?;
    println!("{json}");
    Ok(())
}

fn default_oracle_url(source: &str) -> Option<&'static str> {
    match source {
        "esplora" => Some(esplora::DEFAULT_BASE_URL),
        "luxor" => Some(luxor::DEFAULT_BASE_URL),
        _ => None,
    }
}

/// Convert clap-supplied String into `Option<String>` for profile fields:
/// `Some("")` means "clear", `Some(x)` means "set to x". Wrapped in another
/// Option (`None`) by clap when the flag was omitted entirely.
fn profile_update(v: Option<String>) -> Option<Option<String>> {
    v.map(|s| if s.is_empty() { None } else { Some(s) })
}

fn run_set(args: SetArgs) -> Result<()> {
    let data_dir = resolve_data_dir(args.data_dir)?;
    let path = state_path(&data_dir);
    let mut state = ManifestState::load_from_path(&path)
        .with_context(|| format!("load {}", path.display()))?;

    let mut changed: Vec<String> = Vec::new();

    if let Some(url) = args.instance_url {
        state.instance_url = url;
        changed.push("instance_url".into());
    }

    let source_changed = args.oracle_source.is_some();
    if let Some(src) = args.oracle_source {
        state.hashprice_oracle = src;
        changed.push("hashprice_oracle".into());
    }
    if let Some(url) = args.oracle_url {
        state.hashprice_oracle_url = if url.is_empty() { None } else { Some(url) };
        changed.push("hashprice_oracle_url".into());
    } else if source_changed {
        // Source switched without an explicit URL — reset to the compiled default.
        if let Some(default) = default_oracle_url(&state.hashprice_oracle) {
            state.hashprice_oracle_url = Some(default.to_string());
            changed.push("hashprice_oracle_url (auto)".into());
        }
    }

    if let Some(s) = args.relays {
        let list: Vec<String> = s
            .split(',')
            .map(|r| r.trim().to_string())
            .filter(|r| !r.is_empty())
            .collect();
        state.relays = list;
        changed.push("relays".into());
    }

    if let Some(v) = profile_update(args.name) {
        state.profile.name = v;
        changed.push("profile.name".into());
    }
    if let Some(v) = profile_update(args.about) {
        state.profile.about = v;
        changed.push("profile.about".into());
    }
    if let Some(v) = profile_update(args.nip05) {
        state.profile.nip05 = v;
        changed.push("profile.nip05".into());
    }
    if let Some(v) = profile_update(args.lud16) {
        state.profile.lud16 = v;
        changed.push("profile.lud16".into());
    }
    if let Some(v) = profile_update(args.website) {
        state.profile.website = v;
        changed.push("profile.website".into());
    }

    if changed.is_empty() {
        eprintln!("(no fields specified — nothing changed)");
        return Ok(());
    }

    state
        .save_atomic_to_path(&path)
        .with_context(|| format!("save {}", path.display()))?;

    println!("updated {}:", path.display());
    for f in &changed {
        println!("  - {f}");
    }
    Ok(())
}
