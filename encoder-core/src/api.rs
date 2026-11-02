//! Wire types shared by the daemon and its clients.

use serde::{Deserialize, Serialize};

use crate::{
    ffmpeg::HwCaps,
    job::{Job, JobSpec},
    queue::JobEvent,
};

pub const API_PREFIX: &str = "/api/v1";
pub const DEFAULT_PORT: u16 = 7878;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeInfo {
    pub daemon_version: String,
    pub max_concurrent: usize,
    pub hardware: HwCaps,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitRequest {
    pub jobs: Vec<JobSpec>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SubmitResponse {
    pub jobs: Vec<Job>,
}

/// Messages pushed over the `/events` WebSocket.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived event values; boxing would only add noise
pub enum WsMessage {
    /// Sent once on connect so a client can render current state before deltas arrive.
    Snapshot { jobs: Vec<Job> },
    JobUpdated { job: Job },
    JobRemoved { id: crate::job::JobId },
    /// The client fell behind and missed events; re-fetch the job list.
    Lagged,
}

impl From<JobEvent> for WsMessage {
    fn from(e: JobEvent) -> Self {
        match e {
            JobEvent::Updated(job) => Self::JobUpdated { job: *job },
            JobEvent::Removed(id) => Self::JobRemoved { id },
        }
    }
}
