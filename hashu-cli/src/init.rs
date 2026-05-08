//! `hashu init` — first-run setup.
//!
//! Generates the operator's nostr keypair, prompts interactively for the
//! profile / instance / oracle / relays, and writes both files atomically
//! into the data dir. Refuses to clobber an existing setup.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use clap::Args;
use dialoguer::{Confirm, Input, Select};
use hashu_mint::manifest::{ManifestState, Profile};
use nostr::{Keys, ToBech32};

#[derive(Debug, Args)]
pub struct InitCmd {
    /// Override the data directory. Defaults to $HASHU_DATA_DIR or ~/.hashu.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Skip prompts and accept defaults / blanks. Useful for scripted setup
    /// where profile fields will be edited manually afterwards.
    #[arg(long)]
    pub no_interactive: bool,
}

const DEFAULT_RELAYS: &[&str] = &[
    "wss://relay.damus.io",
    "wss://nos.lol",
    "wss://relay.primal.net",
];

pub fn resolve_data_dir(override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(p) = override_path {
        return Ok(p);
    }
    if let Ok(p) = std::env::var("HASHU_DATA_DIR") {
        return Ok(PathBuf::from(p));
    }
    let home = std::env::var("HOME").map_err(|_| anyhow!("HOME is not set"))?;
    Ok(PathBuf::from(home).join(".hashu"))
}

pub fn nsec_path(data_dir: &Path) -> PathBuf {
    data_dir.join("nostr.nsec")
}

pub fn state_path(data_dir: &Path) -> PathBuf {
    data_dir.join("manifest_state.json")
}

pub async fn run(cmd: InitCmd) -> Result<()> {
    let data_dir = resolve_data_dir(cmd.data_dir)?;
    let nsec = nsec_path(&data_dir);
    let state = state_path(&data_dir);

    if nsec.exists() || state.exists() {
        bail!(
            "data dir already initialized: {}\n  refusing to clobber existing nostr.nsec or manifest_state.json",
            data_dir.display()
        );
    }

    fs::create_dir_all(&data_dir)
        .with_context(|| format!("create {}", data_dir.display()))?;
    let mut perms = fs::metadata(&data_dir)?.permissions();
    perms.set_mode(0o700);
    fs::set_permissions(&data_dir, perms)
        .with_context(|| format!("chmod 0700 {}", data_dir.display()))?;

    let interactive = !cmd.no_interactive;
    let (instance_url, profile, oracle, relays) = if interactive {
        prompt_setup()?
    } else {
        default_setup()
    };

    let mut state_obj = ManifestState::new(instance_url, oracle, relays);
    state_obj.profile = profile;

    let keys = Keys::generate();
    write_nsec(&nsec, &keys)?;
    state_obj
        .save_atomic_to_path(&state)
        .context("save manifest_state.json")?;

    let npub = keys
        .public_key()
        .to_bech32()
        .context("encode npub")?;
    println!("Initialized hashu data dir at {}", data_dir.display());
    println!("  nsec:          {}", nsec.display());
    println!("  manifest:      {}", state.display());
    println!("  npub:          {npub}");
    println!();
    println!("Edit {} to refine profile fields any time.", state.display());
    Ok(())
}

fn prompt_setup() -> Result<(String, Profile, String, Vec<String>)> {
    let instance_url: String = Input::new()
        .with_prompt("Instance URL (e.g. https://mint.example.com)")
        .interact_text()?;

    let name: String = Input::new()
        .with_prompt("Display name (blank to skip)")
        .allow_empty(true)
        .interact_text()?;
    let about: String = Input::new()
        .with_prompt("About (blank to skip)")
        .allow_empty(true)
        .interact_text()?;
    let nip05: String = Input::new()
        .with_prompt("NIP-05 (e.g. op@example.com, blank to skip)")
        .allow_empty(true)
        .interact_text()?;
    let lud16: String = Input::new()
        .with_prompt("Lightning address (LUD-16, blank to skip)")
        .allow_empty(true)
        .interact_text()?;
    let website: String = Input::new()
        .with_prompt("Website URL (blank to skip)")
        .allow_empty(true)
        .interact_text()?;

    let profile = Profile {
        name: nonblank(name),
        about: nonblank(about),
        nip05: nonblank(nip05),
        lud16: nonblank(lud16),
        website: nonblank(website),
    };

    let oracles = ["esplora", "luxor"];
    let oracle_idx = Select::new()
        .with_prompt("Hashprice oracle")
        .items(&oracles)
        .default(0)
        .interact()?;
    let oracle = oracles[oracle_idx].to_string();

    let default_relay_str = DEFAULT_RELAYS.join(", ");
    let relays_input: String = Input::new()
        .with_prompt(format!(
            "Relays (comma-separated, ENTER for default: {default_relay_str})"
        ))
        .allow_empty(true)
        .interact_text()?;
    let relays: Vec<String> = if relays_input.trim().is_empty() {
        DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect()
    } else {
        relays_input
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let confirm = Confirm::new()
        .with_prompt(format!(
            "Generate new nostr keypair and write to disk? (instance={instance_url}, oracle={oracle}, {} relay(s))",
            relays.len()
        ))
        .default(true)
        .interact()?;
    if !confirm {
        bail!("aborted by user");
    }

    Ok((instance_url, profile, oracle, relays))
}

fn default_setup() -> (String, Profile, String, Vec<String>) {
    (
        "https://CHANGE-ME.example.com".to_string(),
        Profile::default(),
        "esplora".to_string(),
        DEFAULT_RELAYS.iter().map(|s| s.to_string()).collect(),
    )
}

fn nonblank(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn write_nsec(path: &Path, keys: &Keys) -> Result<()> {
    let nsec = keys
        .secret_key()
        .to_bech32()
        .context("encode nsec")?;
    let mut f: File = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))?;
    writeln!(f, "{nsec}")?;
    f.sync_all()?;
    Ok(())
}

pub fn load_keys(data_dir: &Path) -> Result<Keys> {
    let path = nsec_path(data_dir);
    let body = fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let nsec = body.trim();
    Keys::parse(nsec).with_context(|| format!("parse nsec at {}", path.display()))
}
