// SPDX-License-Identifier: Apache-2.0

use thiserror::Error;

#[derive(Error, Debug)]
pub enum AgentError {
    #[error("config error: {0}")]
    Config(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("toml parse: {0}")]
    TomlParse(#[from] toml::de::Error),

    #[error("toml serialize: {0}")]
    TomlSerialize(#[from] toml::ser::Error),

    #[error("json: {0}")]
    Json(#[from] serde_json::Error),

    #[error("http: {0}")]
    Http(#[from] reqwest::Error),

    #[error("certificate generation: {0}")]
    Rcgen(#[from] rcgen::Error),

    #[error("tls: {0}")]
    Tls(String),

    #[error("tailscale: {0}")]
    Tailscale(String),

    #[error("registration rejected: {status} {body}")]
    RegisterRejected { status: u16, body: String },

    #[error("heartbeat rejected: {status} {body}")]
    HeartbeatRejected { status: u16, body: String },

    #[error("{0}")]
    Other(String),
}

pub type Result<T, E = AgentError> = std::result::Result<T, E>;
