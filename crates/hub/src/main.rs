use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};
use core_protocol::{ModelEntry, ProviderConfig};
use hub::{db::Db, server};

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
    },
    /// Manage registered devices.
    #[command(subcommand)]
    Device(DeviceCmd),
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let cli = Cli::parse();
    let db = Db::open(&cli.data)?;

    match cli.cmd {
        Cmd::Serve { bind } => {
            let hub = server::Hub::new(db, cli.data.clone());
            // Threads waiting for a device keep waiting; a hub part that was running stops at once.
            hub.recover_waiting_threads();
            // Threads that were running when the hub stopped can't be resumed.
            for t in hub.db.threads(None).into_iter().filter(|t| t.status == "running") {
                hub.db.finish_thread(&t.id, "failed", "The hub restarted while this thread was running.");
            }
            tokio::spawn(hub::usage::collector_loop(hub.clone()));
            let listener = tokio::net::TcpListener::bind(bind).await?;
            tracing::info!("grouper-hub listening on {bind}, data in {}", cli.data.display());
            axum::serve(listener, server::router(hub))
                .with_graceful_shutdown(async {
                    let _ = tokio::signal::ctrl_c().await;
                })
                .await?;
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
            })?;
            println!("Saved model {id}.");
        }
    }
    Ok(())
}
