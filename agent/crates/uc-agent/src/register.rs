// SPDX-License-Identifier: Apache-2.0

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::identity;
use crate::tailnet;

#[derive(Debug, Serialize)]
struct RegisterPayload {
    bootstrap_token: String,
    csr_pem: String,
    facts: Facts,
    requested_tags: Tags,
}

#[derive(Debug, Serialize)]
struct Facts {
    hostname: String,
    tailnet_ip: String,
    os_release: String,
    kernel: String,
    arch: String,
    agent_version: String,
    mcp_port: u32,
    boot_time: String,
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct Tags {
    #[serde(default)]
    tier: String,
    #[serde(default)]
    owner: String,
    #[serde(default)]
    os: String,
}

#[derive(Debug, Deserialize)]
struct RegisterReply {
    device_id: String,
    cert_pem: String,
    ca_chain_pem: String,
    agent_key: String,
}

pub struct RegisterOutcome {
    pub device_id: String,
    pub agent_key: String,
}

pub async fn register(cfg: &Config, identity: &identity::LocalIdentity) -> Result<RegisterOutcome> {
    if cfg.bootstrap.token.is_empty() {
        return Err(AgentError::Config(
            "bootstrap.token is empty; install.sh should populate this".into(),
        ));
    }

    let facts = gather_facts(cfg)?;
    let tags = Tags {
        tier: cfg.bootstrap.tier.clone(),
        owner: cfg.bootstrap.owner.clone(),
        os: detect_os(),
    };

    let payload = RegisterPayload {
        bootstrap_token: cfg.bootstrap.token.clone(),
        csr_pem: identity.csr_pem.clone(),
        facts,
        requested_tags: tags,
    };

    info!(url = %cfg.register_url(), "submitting registration");

    let client = build_client(cfg)?;
    let resp = client
        .post(cfg.register_url())
        .json(&payload)
        .timeout(cfg.agent.register_timeout)
        .send()
        .await?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AgentError::RegisterRejected {
            status: status.as_u16(),
            body,
        });
    }

    let reply: RegisterReply = resp.json().await?;

    identity::write_pem(&cfg.cert_path(), &reply.cert_pem)?;
    identity::write_pem(&cfg.key_path(), &identity.key_pair.serialize_pem())?;
    identity::write_pem(&cfg.ca_chain_path(), &reply.ca_chain_pem)?;

    Ok(RegisterOutcome {
        device_id: reply.device_id,
        agent_key: reply.agent_key,
    })
}

pub fn build_client(cfg: &Config) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .user_agent(format!("uc-agent/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60));
    if cfg.control_plane.tls_skip_verify {
        builder = builder.danger_accept_invalid_certs(true);
    }
    Ok(builder.build()?)
}

fn gather_facts(cfg: &Config) -> Result<Facts> {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();

    let hostname = tailnet::hostname()?;
    let tailnet_ip = tailnet::local_ipv4()?.to_string();
    let os_release = sysinfo::System::long_os_version()
        .unwrap_or_else(|| std::env::consts::OS.to_string());
    let kernel = sysinfo::System::kernel_version().unwrap_or_default();
    let arch = std::env::consts::ARCH.to_string();

    let boot_secs = sysinfo::System::boot_time();
    let boot_time = time::OffsetDateTime::from_unix_timestamp(boot_secs as i64)
        .map_err(|e| AgentError::Other(format!("boot_time parse: {e}")))?;
    let boot_iso = boot_time
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| AgentError::Other(format!("boot_time format: {e}")))?;

    Ok(Facts {
        hostname,
        tailnet_ip,
        os_release,
        kernel,
        arch,
        agent_version: env!("CARGO_PKG_VERSION").to_string(),
        mcp_port: cfg.agent.mcp_port as u32,
        boot_time: boot_iso,
    })
}

fn detect_os() -> String {
    match std::env::consts::OS {
        "linux" => "linux".into(),
        "macos" => "macos".into(),
        other => other.to_string(),
    }
}

