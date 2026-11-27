use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use core_protocol::{ModelEntry, ProviderConfig};
use hub::{backup, db::Db, host};

/// Grouper hub: the always-on server every device connects to.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Folder holding the hub's state.
    #[arg(long, env = "GROUPER_DATA", default_value = "./data", global = true)]
    data: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the server.
    Serve {
        #[arg(long, env = "GROUPER_BIND", default_value = "127.0.0.1:8080")]
        bind: SocketAddr,
        /// Run in local mode (HTTP setup-code claim off, token sign-in loopback-only).
        #[arg(long)]
        local: bool,
    },
    /// Manage registered devices.
    #[command(subcommand)]
    Device(DeviceCmd),
    /// Print a fresh one-time code for claiming this hub with Google (valid 1 hour).
    SetupCode,
    /// The Google account that owns this hub.
    #[command(subcommand)]
    Owner(OwnerCmd),
    /// Add or update a model provider (keys can also be set from any device: `grouper keys set`).
    Provider {
        id: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long, env = "GROUPER_PROVIDER_KEY", hide_env_values = true)]
        key: Option<String>,
        /// JSON options, e.g. '{"script": "/path/script.json"}'
        #[arg(long)]
        options: Option<String>,
    },
    /// Write a consistent snapshot of the database and thread workspaces (works while the hub
    /// runs). The master key is not included: keep it, or GROUPER_MASTER_KEY, separately.
    Backup {
        /// Directory to create, or a path ending in `.tar.gz` / `.tgz`.
        output: PathBuf,
        /// Copy `<data>/workspaces` (pass `--include-workspaces=false` to skip it).
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        include_workspaces: bool,
    },
    /// Restore a snapshot made by `backup` into the data directory.
    Restore {
        /// Directory or `.tar.gz` file produced by `backup`.
        backup: PathBuf,
        /// If the data directory already has hub files, move them aside (never deleted) and restore.
        #[arg(long)]
        force: bool,
    },
    /// Add or update a model in the registry.
    Model {
        id: String,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: String,
        /// Comma-separated: orchestrator, standard, thread
        #[arg(long, default_value = "")]
        roles: String,
        #[arg(long)]
        free: bool,
        #[arg(long, default_value_t = 0.0)]
        input_price: f64,
        #[arg(long, default_value_t = 0.0)]
        output_price: f64,
        #[arg(long)]
        cache_read_price: Option<f64>,
        #[arg(long)]
        cache_write_price: Option<f64>,
        /// Runs on your own hardware (for "sensitive → local" priority rules).
        #[arg(long)]
        local: bool,
    },
    /// Set only the prices (USD per 1M tokens) of an existing model, then re-price its past
    /// unpriced calls. Everything else on the model is kept.
    ModelPrice {
        id: String,
        #[arg(long)]
        input: f64,
        #[arg(long)]
        output: f64,
        #[arg(long)]
        cache_read: Option<f64>,
        #[arg(long)]
        cache_write: Option<f64>,
    },
}

#[derive(Subcommand)]
enum DeviceCmd {
    /// Register a device and print its token (shown once).
    Add {
        name: String,
        #[arg(long, default_value = "unknown")]
        platform: String,
        /// Make this an admin token, for your own client: it can change tool
        /// permissions, answer approvals, manage keys and models, and chat. Don't
        /// give an admin token to a device that runs threads with a shell.
        #[arg(long)]
        admin: bool,
    },
    /// List registered devices.
    List,
    /// Grant admin rights to a device's existing token (or remove them with --off).
    Admin {
        id: String,
        #[arg(long)]
        off: bool,
    },
    /// Revoke a device's token.
    Revoke { id: String },
}

#[derive(Subcommand)]
enum OwnerCmd {
    /// Show the linked Google account.
    Show,
    /// Unlink the owner (if the Google account is lost). Device tokens and sessions keep working.
    Reset,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    // Backup and restore must not open the database through `Db::open`: that migrates, seals keys
    // and can create a master key. A snapshot has to work while `serve` is running, and a restore
    // must not create an empty database before it decides the directory is empty.
    match &cli.cmd {
        Cmd::Backup { output, include_workspaces } => return backup::backup(&cli.data, output, *include_workspaces),
        Cmd::Restore { backup, force } => return backup::restore(&cli.data, backup, *force),
        Cmd::Serve { bind, local } => {
            let handle = host::start_with_options(&cli.data, &[*bind], host::Options { local: *local }).await?;
            let _ = tokio::signal::ctrl_c().await;
            handle.stop().await;
            return Ok(());
        }
        _ => {}
    }
    let db = Db::open(&cli.data)?;

    match cli.cmd {
        Cmd::Backup { .. } | Cmd::Restore { .. } | Cmd::Serve { .. } => {
            unreachable!("handled before opening the database")
        }
        Cmd::Device(DeviceCmd::Add { name, platform, admin }) => {
            let (d, token) = db.add_device(&name, &platform, admin)?;
            let kind = if admin { "admin" } else { "device" };
            println!("Registered {} ({}) as {} with a {kind} token", d.name, d.platform, d.id);
            println!("Token (shown once, keep it secret):\n{token}");
        }
        Cmd::Device(DeviceCmd::List) => {
            for d in db.devices() {
                let state = if d.revoked { "revoked" } else { "active" };
                let kind = if d.admin { "admin" } else { "device" };
                println!("{}  {:<20} {:<8} {:<7} {}", d.id, d.name, d.platform, state, kind);
            }
        }
        Cmd::Device(DeviceCmd::Admin { id, off }) => {
            db.set_admin(&id, !off)?;
            if off {
                println!("{id} now has a plain device token.");
            } else {
                println!("{id} now has an admin token. Don't use it on a device that runs threads with a shell.");
            }
        }
        Cmd::Device(DeviceCmd::Revoke { id }) => {
            db.revoke_device(&id)?;
            println!("Revoked {id}. Restart the hub to drop any live connection, and rotate any API keys it held.");
        }
        Cmd::SetupCode => {
            if let Some(o) = db.owner() {
                anyhow::bail!("this hub is already claimed by {}; `grouper-hub owner reset` unlinks it first", o.email);
            }
            let (code, _) = db.new_setup_code()?;
            println!("Setup code (valid 1 hour, once): {code}");
            println!("Claim the hub: open <hub address>{}", hub::google_auth::claim_path(&code));
        }
        Cmd::Owner(OwnerCmd::Show) => match db.owner() {
            Some(o) => println!("{} {} ({} account {}), since {}", o.email, o.name, o.provider, o.sub, o.claimed_at),
            None => println!("No owner: this hub hasn't been claimed or linked."),
        },
        Cmd::Owner(OwnerCmd::Reset) => match db.reset_owner()? {
            Some(o) => println!(
                "Unlinked {}. Device tokens and sessions keep working; `grouper-hub setup-code` lets a Google account claim the hub again.",
                o.email
            ),
            None => println!("No owner to unlink."),
        },
        Cmd::Provider { id, kind, base_url, key, options } => {
            let options = match options {
                Some(o) => serde_json::from_str(&o)?,
                None => serde_json::Value::Null,
            };
            db.upsert_provider(&ProviderConfig { id: id.clone(), kind, base_url, api_key: key, options })?;
            println!("Saved provider {id}. Connected devices get it when the hub restarts or via `grouper keys set`.");
        }
        Cmd::Model {
            id,
            provider,
            model,
            roles,
            free,
            input_price,
            output_price,
            cache_read_price,
            cache_write_price,
            local,
        } => {
            db.upsert_model(&ModelEntry {
                id: id.clone(),
                provider_id: provider,
                model,
                input_price,
                output_price,
                cache_read_price,
                cache_write_price,
                context_window: 0,
                use_cases: String::new(),
                free,
                roles: roles.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect(),
                local,
                modalities: vec![],
                ..Default::default()
            })?;
            println!("Saved model {id}.");
        }
        Cmd::ModelPrice { id, input, output, cache_read, cache_write } => {
            let mut entry = db.find_model(&id).with_context(|| format!("no model {id}"))?;
            entry.input_price = input;
            entry.output_price = output;
            entry.cache_read_price = cache_read;
            entry.cache_write_price = cache_write;
            entry.price_source = "manual".into();
            db.upsert_model(&entry)?;
            let n = db.reprice_unpriced_usage(&entry)?;
            println!("Set prices for model {id}; re-priced {n} past call(s) that had no cost.");
        }
    }
    Ok(())
}

#[cfg(test)]
mod cli_tests {
    use super::*;

    #[test]
    fn include_workspaces_flag_parses_false() {
        let cli =
            Cli::try_parse_from(["grouper-hub", "--data", "hub-data", "backup", "--include-workspaces=false", "out.tar.gz"])
                .unwrap();
        match cli.cmd {
            Cmd::Backup { include_workspaces, output } => {
                assert!(!include_workspaces);
                assert_eq!(output.as_os_str(), "out.tar.gz");
            }
            _ => panic!("expected backup"),
        }
        assert_eq!(cli.data.as_os_str(), "hub-data");
    }

    #[test]
    fn restore_force_flag_parses() {
        let cli = Cli::try_parse_from(["grouper-hub", "restore", "snap.tar.gz", "--force"]).unwrap();
        match cli.cmd {
            Cmd::Restore { force, backup } => {
                assert!(force);
                assert_eq!(backup.as_os_str(), "snap.tar.gz");
            }
            _ => panic!("expected restore"),
        }
    }
}
