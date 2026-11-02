//! HTTP + WebSocket control API.

use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, Query, Request, State,
    },
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use encoder_core::{
    ffmpeg::{probe_media, FfmpegConfig, MediaProbe},
    api::{NodeInfo, SubmitRequest, SubmitResponse, WsMessage, API_PREFIX},
    queue::QueueError,
    Job, JobId, JobQueue,
};
use subtle::ConstantTimeEq;
use tokio::sync::broadcast::error::RecvError;

use crate::paths::MediaRoots;

#[derive(Clone)]
pub struct AppState {
    pub queue: JobQueue,
    pub roots: MediaRoots,
    pub node: Arc<NodeInfo>,
    pub token: Arc<str>,
    pub ffmpeg: FfmpegConfig,
}

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/node", get(node))
        .route("/jobs", get(list_jobs).post(submit))
        .route("/jobs/{id}", get(get_job).delete(remove_job))
        .route("/jobs/{id}/cancel", post(cancel_job))
        .route("/events", get(events))
        .route("/fs/list", get(fs_list))
        .route("/fs/probe", get(fs_probe))
        .layer(middleware::from_fn_with_state(state.clone(), require_token));

    Router::new()
        .nest(API_PREFIX, Router::new().route("/health", get(|| async { "ok" })).merge(authed))
        .with_state(state)
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let presented = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if bool::from(presented.as_bytes().ct_eq(state.token.as_bytes())) {
        next.run(req).await
    } else {
        ApiError::new(StatusCode::UNAUTHORIZED, "missing or invalid bearer token").into_response()
    }
}

struct ApiError(StatusCode, String);

impl ApiError {
    fn new(status: StatusCode, msg: impl Into<String>) -> Self {
        Self(status, msg.into())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(serde_json::json!({ "error": self.1 }))).into_response()
    }
}

impl From<QueueError> for ApiError {
    fn from(e: QueueError) -> Self {
        let status = match e {
            QueueError::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::CONFLICT,
        };
        Self::new(status, e.to_string())
    }
}

fn parse_id(s: &str) -> Result<JobId, ApiError> {
    s.parse().map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "invalid job id"))
}

async fn node(State(s): State<AppState>) -> Json<NodeInfo> {
    Json((*s.node).clone())
}

async fn list_jobs(State(s): State<AppState>) -> Json<Vec<Job>> {
    Json(s.queue.list())
}

async fn get_job(State(s): State<AppState>, Path(id): Path<String>) -> Result<Json<Job>, ApiError> {
    s.queue
        .get(parse_id(&id)?)
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "job not found"))
}

async fn submit(
    State(s): State<AppState>,
    Json(req): Json<SubmitRequest>,
) -> Result<(StatusCode, Json<SubmitResponse>), ApiError> {
    if req.jobs.is_empty() {
        return Err(ApiError::new(StatusCode::BAD_REQUEST, "no jobs in request"));
    }
    // Validate the whole batch before queueing any of it.
    let specs = req
        .jobs
        .iter()
        .enumerate()
        .map(|(i, spec)| {
            s.roots
                .validate(spec)
                .map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("job {i}: {e}")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut jobs = Vec::with_capacity(specs.len());
    for spec in specs {
        jobs.push(s.queue.submit(spec).await);
    }
    Ok((StatusCode::ACCEPTED, Json(SubmitResponse { jobs })))
}

async fn cancel_job(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    s.queue.cancel(parse_id(&id)?).await?;
    Ok(StatusCode::ACCEPTED)
}

async fn remove_job(State(s): State<AppState>, Path(id): Path<String>) -> Result<StatusCode, ApiError> {
    s.queue.remove(parse_id(&id)?).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(serde::Deserialize)]
struct ListQuery {
    path: Option<String>,
}

async fn fs_list(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<encoder_core::fsutil::DirListing>, ApiError> {
    let path = q.path.filter(|p| !p.is_empty()).map(std::path::PathBuf::from);
    // Network shares can be slow; keep the runtime free.
    let listing = tokio::task::spawn_blocking(move || s.roots.list(path.as_deref()))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
        .map_err(|e| ApiError::new(StatusCode::FORBIDDEN, e))?;
    Ok(Json(listing))
}

async fn fs_probe(
    State(s): State<AppState>,
    Query(q): Query<ListQuery>,
) -> Result<Json<MediaProbe>, ApiError> {
    let path = q.path.ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "path is required"))?;
    let checked = {
        let roots = s.roots.clone();
        tokio::task::spawn_blocking(move || roots.check_file(std::path::Path::new(&path)))
            .await
            .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?
            .map_err(|e| ApiError::new(StatusCode::FORBIDDEN, e))?
    };
    probe_media(&s.ffmpeg, &checked)
        .await
        .map(Json)
        .ok_or_else(|| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, "not readable video media"))
}

async fn events(State(s): State<AppState>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| stream_events(socket, s.queue))
}

async fn stream_events(mut socket: WebSocket, queue: JobQueue) {
    // Subscribe before taking the snapshot so no update falls between the two.
    let mut rx = queue.subscribe();
    if send(&mut socket, &WsMessage::Snapshot { jobs: queue.list() }).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            event = rx.recv() => {
                let msg = match event {
                    Ok(e) => WsMessage::from(e),
                    Err(RecvError::Lagged(_)) => WsMessage::Lagged,
                    Err(RecvError::Closed) => return,
                };
                if send(&mut socket, &msg).await.is_err() {
                    return;
                }
            }
            incoming = socket.recv() => match incoming {
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                // Axum answers pings itself; the protocol is server-push only.
                Some(Ok(_)) => {}
            },
        }
    }
}

async fn send(socket: &mut WebSocket, msg: &WsMessage) -> Result<(), axum::Error> {
    let text = serde_json::to_string(msg).expect("WsMessage serializes");
    socket.send(Message::Text(text.into())).await
}

#[cfg(test)]
mod tests {
    use axum::{body::Body, http::Request};
    use encoder_core::{
        executor::{BoxFuture, CancellationToken, ExecError, Executor, ProgressFn},
        ffmpeg::HwCaps,
        JobSpec, QueueConfig,
    };
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    struct Hang;
    impl Executor for Hang {
        fn execute(&self, _: JobSpec, _: ProgressFn, cancel: CancellationToken) -> BoxFuture<'_, Result<(), ExecError>> {
            Box::pin(async move {
                cancel.cancelled().await;
                Err(ExecError::Cancelled)
            })
        }
    }

    async fn app() -> (Router, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("encraft-api-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("in.mkv"), b"x").unwrap();
        let state = AppState {
            queue: JobQueue::start(QueueConfig::default(), Arc::new(Hang)).await,
            roots: MediaRoots::new(std::slice::from_ref(&dir)).unwrap(),
            node: Arc::new(NodeInfo {
                daemon_version: "test".into(),
                max_concurrent: 1,
                hardware: HwCaps::default(),
            }),
            token: "secret-token".into(),
            ffmpeg: FfmpegConfig::default(),
        };
        (router(state), dir)
    }

    fn req(method: &str, uri: &str, token: Option<&str>, body: Body) -> Request<Body> {
        let mut b = Request::builder().method(method).uri(uri).header("content-type", "application/json");
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        b.body(body).unwrap()
    }

    #[tokio::test]
    async fn health_is_open_everything_else_needs_token() {
        let (app, _) = app().await;
        let r = app.clone().oneshot(req("GET", "/api/v1/health", None, Body::empty())).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        for token in [None, Some("wrong")] {
            let r = app.clone().oneshot(req("GET", "/api/v1/jobs", token, Body::empty())).await.unwrap();
            assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        }
        let r = app.oneshot(req("GET", "/api/v1/jobs", Some("secret-token"), Body::empty())).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn fs_list_is_confined_to_roots() {
        let (app, dir) = app().await;
        let get = |uri: String| req("GET", &uri, Some("secret-token"), Body::empty());
        let enc = |p: &std::path::Path| p.to_string_lossy().replace('\\', "%5C").replace(':', "%3A").replace(' ', "%20");

        let r = app.clone().oneshot(get("/api/v1/fs/list".into())).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK); // lists the roots

        let r = app.clone().oneshot(get(format!("/api/v1/fs/list?path={}", enc(&dir)))).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let l: encoder_core::fsutil::DirListing = serde_json::from_slice(&body).unwrap();
        assert!(l.entries.iter().any(|e| e.name == "in.mkv"));

        let outside = std::env::temp_dir();
        let r = app.clone().oneshot(get(format!("/api/v1/fs/list?path={}", enc(&outside)))).await.unwrap();
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        let r = app.clone().oneshot(get(format!("/api/v1/fs/probe?path={}", enc(&outside.join("x.mov"))))).await.unwrap();
        assert!(matches!(r.status(), StatusCode::FORBIDDEN), "probe must be confined to media roots, got {}", r.status());

        let r = app.oneshot(req("GET", "/api/v1/fs/list", None, Body::empty())).await.unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn submit_validates_paths_then_queues() {
        let (app, dir) = app().await;
        let good = serde_json::json!({"jobs":[{
            "input": dir.join("in.mkv"), "output": dir.join("out.mp4"), "video": "h264"
        }]});
        let r = app
            .clone()
            .oneshot(req("POST", "/api/v1/jobs", Some("secret-token"), Body::from(good.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::ACCEPTED);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        let resp: SubmitResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(resp.jobs.len(), 1);

        let bad = serde_json::json!({"jobs":[{
            "input": "/etc/passwd", "output": dir.join("x.mp4"), "video": "h264"
        }]});
        let r = app
            .oneshot(req("POST", "/api/v1/jobs", Some("secret-token"), Body::from(bad.to_string())))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
