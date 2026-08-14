// SPDX-License-Identifier: Apache-2.0

//! UUDA agent entry point. Per-device daemon that registers with
//! ops-center, heartbeats every 30s, and exposes a typed MCP tool
//! surface over mTLS on the tailnet at :47000.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing::{error, info};

mod config;
mod error;
mod heartbeat;
mod identity;
mod mcp;
mod register;
mod server;
mod tailnet;

use config::{Config, Registration, DEFAULT_CONFIG_PATH};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("rustls CryptoProvider already installed"))?;

    let config_path = std::env::var("UC_AGENT_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_CONFIG_PATH));

    info!(
        version = uc_types::AGENT_VERSION,
        config = %config_path.display(),
        "uc-agent starting"
    );

    let cfg = Arc::new(
        Config::load(&config_path)
            .with_context(|| format!("loading config at {}", config_path.display()))?,
    );

    let registration = ensure_registered(&cfg).await?;

    if let Ok(cert) = std::fs::read_to_string(cfg.cert_path()) {
        identity::warn_if_expiring_soon(&cert);
    }

    let listen_ip = tailnet::local_ipv4()?;
    info!(ip = %listen_ip, port = cfg.agent.mcp_port, "binding mTLS listener");

    let sampler = uc_toolkit::metrics::Sampler::new();
    sampler.clone().start();

    let cfg_for_server = Arc::clone(&cfg);
    let sampler_for_server = sampler.clone();
    let server_handle = tokio::spawn(async move {
        if let Err(e) = server::run(cfg_for_server, listen_ip, sampler_for_server).await {
            error!(error = %e, "mTLS server stopped");
        }
    });

    let hb = heartbeat::HeartbeatService::new(Arc::clone(&cfg), registration);
    let hb_handle = tokio::spawn(async move {
        if let Err(e) = hb.run().await {
            error!(error = %e, "heartbeat loop stopped");
        }
    });

    tokio::select! {
        _ = server_handle => {
            error!("mTLS server task exited");
        }
        _ = hb_handle => {
            error!("heartbeat task exited");
        }
        _ = shutdown_signal() => {
            info!("shutdown signal received");
        }
    }

    Ok(())
}

fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .json()
        .init();
}

async fn ensure_registered(cfg: &Config) -> anyhow::Result<Registration> {
    if let Some(existing) = Registration::load(&cfg.registration_path())? {
        info!(device_id = %existing.device_id, "using existing registration");
        return Ok(existing);
    }

    info!("no prior registration found; registering with control plane");

    let common_name = tailnet::hostname()?;
    let local = identity::fresh_identity(&common_name)?;
    let outcome = register::register(cfg, &local).await?;

    let now = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default();
    let registration = Registration {
        device_id: outcome.device_id.clone(),
        agent_key: outcome.agent_key,
        registered_at: now,
    };
    registration.save(&cfg.registration_path())?;

    info!(device_id = %outcome.device_id, "registration complete");
    Ok(registration)
}

async fn shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
    let mut int = signal(SignalKind::interrupt()).expect("install SIGINT handler");
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}
