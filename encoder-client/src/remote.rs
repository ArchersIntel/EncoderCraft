//! Client for a node's control API (HTTP + WebSocket). Always talks plain HTTP to a loopback
//! address: for remote nodes that address is the local end of an SSH [`Tunnel`](crate::ssh::Tunnel).

use std::{net::SocketAddr, time::Duration};

use encoder_core::{
    api::{NodeInfo, SubmitRequest, SubmitResponse, WsMessage, API_PREFIX},
    Job, JobId, JobSpec,
};
use futures_util::StreamExt;
use tokio::{sync::mpsc::UnboundedSender, task::JoinHandle};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, Message};

#[derive(Debug, thiserror::Error)]
pub enum RemoteError {
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("{0}")]
    Api(String),
}

#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // short-lived event values; boxing would only add noise
pub enum RemoteEvent {
    Connected,
    Disconnected,
    Message(WsMessage),
}

#[derive(Clone)]
pub struct RemoteClient {
    http: reqwest::Client,
    addr: SocketAddr,
    token: String,
}

impl RemoteClient {
    pub fn new(addr: SocketAddr, token: String) -> Self {
        Self { http: reqwest::Client::new(), addr, token }
    }

    fn url(&self, path: &str) -> String {
        format!("http://{}{API_PREFIX}{path}", self.addr)
    }

    async fn check(resp: reqwest::Response) -> Result<reqwest::Response, RemoteError> {
        if resp.status().is_success() {
            return Ok(resp);
        }
        let status = resp.status();
        let msg = resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        Err(RemoteError::Api(msg))
    }

    pub async fn node_info(&self) -> Result<NodeInfo, RemoteError> {
        let r = self.http.get(self.url("/node")).bearer_auth(&self.token).send().await?;
        Ok(Self::check(r).await?.json().await?)
    }

    /// Lists a folder on the node (inside its media roots); `None` lists the roots.
    pub async fn list_dir(&self, path: Option<&str>) -> Result<encoder_core::fsutil::DirListing, RemoteError> {
        let mut req = self.http.get(self.url("/fs/list")).bearer_auth(&self.token);
        if let Some(p) = path {
            req = req.query(&[("path", p)]);
        }
        Ok(Self::check(req.send().await?).await?.json().await?)
    }

    /// Probes a video file on the node. `Ok(None)` if it isn't readable media.
    pub async fn probe(&self, path: &str) -> Result<Option<encoder_core::ffmpeg::MediaProbe>, RemoteError> {
        let r = self.http.get(self.url("/fs/probe")).bearer_auth(&self.token).query(&[("path", path)]).send().await?;
        if r.status() == reqwest::StatusCode::UNPROCESSABLE_ENTITY {
            return Ok(None);
        }
        Ok(Some(Self::check(r).await?.json().await?))
    }

    pub async fn list_jobs(&self) -> Result<Vec<Job>, RemoteError> {
        let r = self.http.get(self.url("/jobs")).bearer_auth(&self.token).send().await?;
        Ok(Self::check(r).await?.json().await?)
    }

    pub async fn submit(&self, jobs: Vec<JobSpec>) -> Result<Vec<Job>, RemoteError> {
        let r = self
            .http
            .post(self.url("/jobs"))
            .bearer_auth(&self.token)
            .json(&SubmitRequest { jobs })
            .send()
            .await?;
        let resp: SubmitResponse = Self::check(r).await?.json().await?;
        Ok(resp.jobs)
    }

    pub async fn cancel(&self, id: JobId) -> Result<(), RemoteError> {
        let r = self.http.post(self.url(&format!("/jobs/{id}/cancel"))).bearer_auth(&self.token).send().await?;
        Self::check(r).await.map(|_| ())
    }

    pub async fn remove(&self, id: JobId) -> Result<(), RemoteError> {
        let r = self.http.delete(self.url(&format!("/jobs/{id}"))).bearer_auth(&self.token).send().await?;
        Self::check(r).await.map(|_| ())
    }

    /// Streams events until the task is aborted, reconnecting with backoff. Each (re)connect
    /// begins with a `Snapshot` message, so consumers never need to patch up gaps themselves.
    pub fn spawn_event_stream(&self, tx: UnboundedSender<RemoteEvent>) -> JoinHandle<()> {
        let (addr, token) = (self.addr, self.token.clone());
        tokio::spawn(async move {
            let mut backoff = Duration::from_millis(500);
            loop {
                if let Ok(()) = stream_once(addr, &token, &tx, &mut backoff).await {
                    // fallthrough: clean close, retry
                }
                if tx.send(RemoteEvent::Disconnected).is_err() {
                    return;
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(10));
            }
        })
    }
}

async fn stream_once(
    addr: SocketAddr,
    token: &str,
    tx: &UnboundedSender<RemoteEvent>,
    backoff: &mut Duration,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut req = format!("ws://{addr}{API_PREFIX}/events").into_client_request()?;
    req.headers_mut().insert("Authorization", HeaderValue::from_str(&format!("Bearer {token}"))?);
    let (mut ws, _) = tokio_tungstenite::connect_async(req).await?;
    *backoff = Duration::from_millis(500);
    let _ = tx.send(RemoteEvent::Connected);
    while let Some(msg) = ws.next().await {
        match msg? {
            Message::Text(text) => {
                if let Ok(m) = serde_json::from_str::<WsMessage>(text.as_str()) {
                    if tx.send(RemoteEvent::Message(m)).is_err() {
                        return Ok(());
                    }
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }
    Ok(())
}
