//! A connected node: either the in-process local queue or a remote daemon behind an SSH tunnel.

use std::sync::Arc;

use encoder_client::{
    remote::{RemoteClient, RemoteEvent},
    ssh::{SshSession, Tunnel},
};
use encoder_core::{
    api::{NodeInfo, WsMessage},
    ffmpeg::{probe_media, FfmpegConfig, MediaProbe},
    fsutil::{list_dir, DirListing},
    Job, JobEvent, JobId, JobQueue, JobSpec,
};
use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tokio::{sync::broadcast::error::RecvError, task::JoinHandle};

#[derive(Serialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)] // short-lived event values; boxing would only add noise
pub enum UiEvent {
    Snapshot { jobs: Vec<Job> },
    JobUpdated { job: Job },
    JobRemoved { id: JobId },
    Connection { connected: bool },
    Lagged,
}

#[derive(Serialize, Clone)]
struct NodeEvent {
    node_id: String,
    #[serde(flatten)]
    event: UiEvent,
}

fn emit(app: &AppHandle, node_id: &str, event: UiEvent) {
    let _ = app.emit("node-event", NodeEvent { node_id: node_id.to_owned(), event });
}

impl From<WsMessage> for UiEvent {
    fn from(m: WsMessage) -> Self {
        match m {
            WsMessage::Snapshot { jobs } => Self::Snapshot { jobs },
            WsMessage::JobUpdated { job } => Self::JobUpdated { job },
            WsMessage::JobRemoved { id } => Self::JobRemoved { id },
            WsMessage::Lagged => Self::Lagged,
        }
    }
}

enum Backend {
    Local(JobQueue),
    Remote {
        client: RemoteClient,
        // Held so the SSH connection and tunnel live as long as the runtime.
        _session: Arc<SshSession>,
        _tunnel: Tunnel,
    },
}

pub struct Runtime {
    pub info: NodeInfo,
    local_ffmpeg: FfmpegConfig,
    backend: Backend,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Runtime {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Runtime {
    pub fn local(app: AppHandle, queue: JobQueue, info: NodeInfo, node_id: String, ffmpeg: FfmpegConfig) -> Self {
        let mut rx = queue.subscribe();
        let q = queue.clone();
        let task = tokio::spawn(async move {
            emit(&app, &node_id, UiEvent::Connection { connected: true });
            emit(&app, &node_id, UiEvent::Snapshot { jobs: q.list() });
            loop {
                let ev = match rx.recv().await {
                    Ok(JobEvent::Updated(job)) => UiEvent::JobUpdated { job: *job },
                    Ok(JobEvent::Removed(id)) => UiEvent::JobRemoved { id },
                    Err(RecvError::Lagged(_)) => UiEvent::Lagged,
                    Err(RecvError::Closed) => return,
                };
                emit(&app, &node_id, ev);
            }
        });
        Self { info, local_ffmpeg: ffmpeg, backend: Backend::Local(queue), tasks: vec![task] }
    }

    pub fn remote(
        app: AppHandle,
        node_id: String,
        client: RemoteClient,
        info: NodeInfo,
        session: Arc<SshSession>,
        tunnel: Tunnel,
    ) -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let stream = client.spawn_event_stream(tx);
        let forward = tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                let ev = match ev {
                    RemoteEvent::Connected => UiEvent::Connection { connected: true },
                    RemoteEvent::Disconnected => UiEvent::Connection { connected: false },
                    RemoteEvent::Message(m) => m.into(),
                };
                emit(&app, &node_id, ev);
            }
        });
        Self { info, local_ffmpeg: FfmpegConfig::default(), backend: Backend::Remote { client, _session: session, _tunnel: tunnel }, tasks: vec![stream, forward] }
    }

    pub async fn list(&self) -> Result<Vec<Job>, String> {
        match &self.backend {
            Backend::Local(q) => Ok(q.list()),
            Backend::Remote { client, .. } => client.list_jobs().await.map_err(|e| e.to_string()),
        }
    }

    pub async fn submit(&self, specs: Vec<JobSpec>) -> Result<Vec<Job>, String> {
        match &self.backend {
            Backend::Local(q) => {
                let mut jobs = Vec::with_capacity(specs.len());
                for spec in specs {
                    jobs.push(q.submit(spec).await);
                }
                Ok(jobs)
            }
            Backend::Remote { client, .. } => client.submit(specs).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn cancel(&self, id: JobId) -> Result<(), String> {
        match &self.backend {
            Backend::Local(q) => q.cancel(id).await.map_err(|e| e.to_string()),
            Backend::Remote { client, .. } => client.cancel(id).await.map_err(|e| e.to_string()),
        }
    }

    pub async fn remove(&self, id: JobId) -> Result<(), String> {
        match &self.backend {
            Backend::Local(q) => q.remove(id).await.map_err(|e| e.to_string()),
            Backend::Remote { client, .. } => client.remove(id).await.map_err(|e| e.to_string()),
        }
    }

    /// Local: any folder (default: the user's home). Remote: inside the server's media roots.
    pub async fn list_dir(&self, path: Option<String>) -> Result<DirListing, String> {
        match &self.backend {
            Backend::Local(_) => {
                let dir = path.filter(|p| !p.is_empty()).map(std::path::PathBuf::from).or_else(|| {
                    std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")).map(Into::into)
                });
                let dir = dir.ok_or("no folder given")?;
                tokio::task::spawn_blocking(move || list_dir(&dir).map_err(|e| format!("{}: {e}", dir.display())))
                    .await
                    .map_err(|e| e.to_string())?
            }
            Backend::Remote { client, .. } => client.list_dir(path.as_deref()).await.map_err(|e| e.to_string()),
        }
    }

    /// Probes a video file on this node; `None` if it is not readable media.
    pub async fn probe(&self, path: &str) -> Option<MediaProbe> {
        match &self.backend {
            Backend::Local(_) => probe_media(&self.local_ffmpeg, std::path::Path::new(path)).await,
            Backend::Remote { client, .. } => client.probe(path).await.ok().flatten(),
        }
    }

    pub fn shutdown(&self) {
        if let Backend::Local(q) = &self.backend {
            q.shutdown();
        }
    }
}
