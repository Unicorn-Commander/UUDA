// SPDX-License-Identifier: Apache-2.0

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use axum::{routing::get, Json, Router};
use axum_server::tls_rustls::RustlsConfig;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rustls::server::WebPkiClientVerifier;
use rustls::ServerConfig;
use rustls_pki_types::CertificateDer;
use serde_json::json;
use tracing::info;
use uc_toolkit::metrics::Sampler;

use crate::config::Config;
use crate::error::{AgentError, Result};
use crate::mcp::UudaServer;

pub async fn run(cfg: Arc<Config>, ip: Ipv4Addr, sampler: Sampler) -> Result<()> {
    let tls = build_tls(&cfg)?;
    let addr = SocketAddr::from((ip, cfg.agent.mcp_port));

    let agent_version = env!("CARGO_PKG_VERSION").to_string();
    let state_dir = cfg.paths.state_dir.clone();
    let has_local_agent = cfg.agent.has_local_agent;
    let messages_enabled = cfg.agent.messages_enabled;
    let messages_db_path = cfg
        .agent
        .messages_db_path
        .clone()
        .unwrap_or_else(uc_toolkit::messages::default_db_path);
    let files_enabled = cfg.files_enabled();
    let files_roots = cfg.files_roots();
    let files_index_path = cfg.files_index_path();
    let mcp_service = StreamableHttpService::new(
        {
            let sampler = sampler.clone();
            let agent_version = agent_version.clone();
            let state_dir = state_dir.clone();
            let messages_db_path = messages_db_path.clone();
            let files_roots = files_roots.clone();
            let files_index_path = files_index_path.clone();
            move || {
                Ok(UudaServer::new(
                    sampler.clone(),
                    agent_version.clone(),
                    state_dir.clone(),
                    has_local_agent,
                    messages_enabled,
                    messages_db_path.clone(),
                    files_enabled,
                    files_roots.clone(),
                    files_index_path.clone(),
                ))
            }
        },
        LocalSessionManager::default().into(),
        // UUDA is a request/response tool surface — federation clients (Customer-Ops,
        // Email-Ops, …) POST a bare `tools/call` with no `initialize`/session handshake.
        // Stateful mode (the rmcp default) rejects that with 422 "expect initialize
        // request", so the agent must run STATELESS. json_response also drops the SSE
        // framing for plain `application/json` (MCP Streamable HTTP spec 2025-06-18).
        // No tool needs server-push streaming; messages.poll is client-pull.
        StreamableHttpServerConfig::default()
            .with_stateful_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts(),
    );

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/version", get(version))
        .nest_service("/mcp/v1", mcp_service);

    info!(addr = %addr, "mTLS listener with MCP routes");

    axum_server::bind_rustls(addr, RustlsConfig::from_config(Arc::new(tls)))
        .serve(app.into_make_service())
        .await
        .map_err(|e| AgentError::Tls(format!("axum_server: {e}")))?;
    Ok(())
}

fn build_tls(cfg: &Config) -> Result<ServerConfig> {
    let cert_pem = std::fs::read(cfg.cert_path())?;
    let key_pem = std::fs::read(cfg.key_path())?;
    let ca_pem = std::fs::read(cfg.ca_chain_path())?;

    let mut cert_slice = cert_pem.as_slice();
    let cert_chain = rustls_pemfile::certs(&mut cert_slice)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| AgentError::Tls(format!("cert parse: {e}")))?;
    if cert_chain.is_empty() {
        return Err(AgentError::Tls("empty cert chain".into()));
    }

    let mut key_slice = key_pem.as_slice();
    let mut key_iter = rustls_pemfile::pkcs8_private_keys(&mut key_slice);
    let key = key_iter
        .next()
        .ok_or_else(|| AgentError::Tls("no pkcs8 key found".into()))?
        .map_err(|e| AgentError::Tls(format!("key parse: {e}")))?;

    let mut ca_slice = ca_pem.as_slice();
    let ca_certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut ca_slice)
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| AgentError::Tls(format!("ca parse: {e}")))?;

    let mut roots = rustls::RootCertStore::empty();
    for c in ca_certs {
        roots
            .add(c)
            .map_err(|e| AgentError::Tls(format!("trust anchor: {e}")))?;
    }

    let verifier = WebPkiClientVerifier::builder(Arc::new(roots))
        .build()
        .map_err(|e| AgentError::Tls(format!("client verifier: {e}")))?;

    let server = ServerConfig::builder()
        .with_client_cert_verifier(verifier)
        .with_single_cert(cert_chain, key.into())
        .map_err(|e| AgentError::Tls(format!("server config: {e}")))?;
    Ok(server)
}

async fn healthz() -> Json<serde_json::Value> {
    Json(json!({"status": "ok", "version": env!("CARGO_PKG_VERSION")}))
}

async fn version() -> Json<serde_json::Value> {
    Json(json!({"agent_version": env!("CARGO_PKG_VERSION"), "proto_package": uc_types::PROTO_PACKAGE}))
}
