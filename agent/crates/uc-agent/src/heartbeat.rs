// SPDX-License-Identifier: Apache-2.0

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::config::{Config, Registration};
use crate::error::{AgentError, Result};
use crate::register;

const QUEUE_CAPACITY: usize = 1000;

#[derive(Debug, Clone, Serialize)]
struct HeartbeatPayload {
    device_id: String,
    agent_version: String,
    uptime_secs: u64,
    declared_state_digest: String,
}

#[derive(Debug, Deserialize)]
struct HeartbeatReply {
    #[serde(default)]
    reregister_required: bool,
}

pub struct HeartbeatService {
    cfg: Arc<Config>,
    registration: Registration,
    started_at: Instant,
    queue: Arc<Mutex<VecDeque<HeartbeatPayload>>>,
}

impl HeartbeatService {
    pub fn new(cfg: Arc<Config>, registration: Registration) -> Self {
        Self {
            cfg,
            registration,
            started_at: Instant::now(),
            queue: Arc::new(Mutex::new(VecDeque::with_capacity(QUEUE_CAPACITY))),
        }
    }

    pub async fn run(self) -> Result<()> {
        let client = register::build_client(&self.cfg)?;
        let mut ticker = tokio::time::interval(self.cfg.agent.heartbeat_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            ticker.tick().await;
            let payload = self.next_payload();
            self.enqueue(payload).await;
            if let Err(e) = self.drain(&client).await {
                warn!(error = %e, "heartbeat drain failed; retaining queue");
            }
        }
    }

    fn next_payload(&self) -> HeartbeatPayload {
        HeartbeatPayload {
            device_id: self.registration.device_id.clone(),
            agent_version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_secs: self.started_at.elapsed().as_secs(),
            declared_state_digest: String::new(),
        }
    }

    async fn enqueue(&self, payload: HeartbeatPayload) {
        let mut q = self.queue.lock().await;
        if q.len() >= QUEUE_CAPACITY {
            q.pop_front();
        }
        q.push_back(payload);
    }

    async fn drain(&self, client: &reqwest::Client) -> Result<()> {
        let mut q = self.queue.lock().await;
        while let Some(payload) = q.front().cloned() {
            drop(q);
            send_one(client, &self.cfg, &self.registration.agent_key, &payload).await?;
            debug!(uptime = payload.uptime_secs, "heartbeat sent");
            q = self.queue.lock().await;
            q.pop_front();
        }
        Ok(())
    }
}

async fn send_one(
    client: &reqwest::Client,
    cfg: &Config,
    agent_key: &str,
    payload: &HeartbeatPayload,
) -> Result<()> {
    let resp = client
        .post(cfg.heartbeat_url())
        .header("X-Agent-Key", agent_key)
        .json(payload)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(AgentError::HeartbeatRejected {
            status: status.as_u16(),
            body,
        });
    }
    let reply: HeartbeatReply = resp.json().await.unwrap_or(HeartbeatReply {
        reregister_required: false,
    });
    if reply.reregister_required {
        info!("control plane requested re-registration");
    }
    Ok(())
}
