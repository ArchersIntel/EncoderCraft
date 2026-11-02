//! EncoderCraft desktop app (Tauri). The UI in `ui/` talks to these commands; all SSH, queue
//! and network logic lives in `encoder-client` / `encoder-core`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod nodes;
mod runtime;

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use encoder_client::{
    provision::{provision, ProvisionConfig, Secret},
    remote::RemoteClient,
    ssh::{probe_host_key, SshAuth, SshSession, SshTarget, Tunnel},
};
use encoder_core::{
    api::NodeInfo,
    ffmpeg::{FfmpegConfig, FfmpegExecutor, HwCaps},
    Job, JobId, JobQueue, JobSpec, QueueConfig,
};
use nodes::{secrets, NodeRecord, NodeStore, LOCAL_ID};
use runtime::Runtime;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, RunEvent, State};
use tokio::sync::RwLock;

/// Sends FilmCraft project sequences to FilmCraft and everything else to FFmpeg.
struct RoutingExecutor {
    ffmpeg: FfmpegExecutor,
    filmcraft: encoder_filmcraft::FilmcraftExecutor,
}

impl encoder_core::executor::Executor for RoutingExecutor {
    fn execute(
        &self,
        spec: JobSpec,
        on_progress: encoder_core::executor::ProgressFn,
        cancel: encoder_core::executor::CancellationToken,
    ) -> encoder_core::executor::BoxFuture<'_, Result<(), encoder_core::executor::ExecError>> {
        if spec.filmcraft.is_some() {
            self.filmcraft.execute(spec, on_progress, cancel)
        } else {
            self.ffmpeg.execute(spec, on_progress, cancel)
        }
    }
}

#[derive(Serialize)]
struct FilmcraftInfo {
    found: bool,
    path: Option<String>,
    version: Option<String>,
}

#[tauri::command]
async fn filmcraft_info() -> FilmcraftInfo {
    match encoder_filmcraft::find_cli() {
        Some(p) => FilmcraftInfo {
            found: true,
            version: encoder_filmcraft::cli_version(&p).await,
            path: Some(p.display().to_string()),
        },
        None => FilmcraftInfo { found: false, path: None, version: None },
    }
}

struct AppState {
    store: std::sync::Mutex<NodeStore>,
    runtimes: RwLock<HashMap<String, Arc<Runtime>>>,
}

#[derive(Serialize)]
struct NodeView {
    id: String,
    name: String,
    kind: &'static str,
    host: Option<String>,
    username: Option<String>,
    uses_key: bool,
    connected: bool,
    info: Option<NodeInfo>,
}

impl AppState {
    async fn view(&self, rec: Option<&NodeRecord>) -> NodeView {
        let id = rec.map_or(LOCAL_ID, |r| r.id.as_str());
        let rt = self.runtimes.read().await.get(id).cloned();
        NodeView {
            id: id.to_owned(),
            name: rec.map_or("Local Machine".to_owned(), |r| r.name.clone()),
            kind: if rec.is_some() { "remote" } else { "local" },
            host: rec.map(|r| r.host.clone()),
            username: rec.map(|r| r.username.clone()),
            uses_key: rec.is_some_and(|r| r.key_path.is_some()),
            connected: rt.is_some(),
            info: rt.map(|r| r.info.clone()),
        }
    }

    async fn runtime(&self, node_id: &str) -> Result<Arc<Runtime>, String> {
        self.runtimes
            .read()
            .await
            .get(node_id)
            .cloned()
            .ok_or_else(|| "node is not connected".to_owned())
    }
}

#[tauri::command]
async fn list_nodes(state: State<'_, AppState>) -> Result<Vec<NodeView>, String> {
    let records = state.store.lock().unwrap_or_else(|e| e.into_inner()).nodes.clone();
    let mut out = vec![state.view(None).await];
    for r in &records {
        out.push(state.view(Some(r)).await);
    }
    Ok(out)
}

#[tauri::command]
async fn probe_host(host: String, port: u16) -> Result<String, String> {
    probe_host_key(host.trim(), port).await.map_err(|e| e.to_string())
}

#[tauri::command]
fn default_daemon_binary() -> Option<String> {
    // Dev override: point at a freshly built binary (see scripts/build-daemon-linux.ps1).
    if let Some(p) = std::env::var_os("ENCRAFT_DAEMON_BINARY").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p.display().to_string());
    }
    let exe = std::env::current_exe().ok()?;
    let candidates = [
        exe.parent()?.join("resources").join("encraft-d-linux-x86_64"),
        exe.parent()?.join("encraft-d-linux-x86_64"),
    ];
    candidates.into_iter().find(|p| p.is_file()).map(|p| p.display().to_string())
}

fn ssh_auth(rec_key: Option<&PathBuf>, secret: Option<String>) -> Result<SshAuth, String> {
    match rec_key {
        Some(path) => Ok(SshAuth::KeyFile { path: path.clone(), passphrase: secret.filter(|s| !s.is_empty()) }),
        None => secret.map(SshAuth::Password).ok_or_else(|| "password required".to_owned()),
    }
}

async fn attach_remote(
    app: &AppHandle,
    state: &AppState,
    rec: &NodeRecord,
    token: String,
    session: Arc<SshSession>,
) -> Result<(), String> {
    let tunnel = Tunnel::start(session.clone(), rec.remote_port).await.map_err(|e| e.to_string())?;
    let client = RemoteClient::new(tunnel.local_addr, token);
    let info = tokio::time::timeout(Duration::from_secs(10), client.node_info())
        .await
        .map_err(|_| "daemon did not answer within 10s".to_owned())?
        .map_err(|e| e.to_string())?;
    let rt = Runtime::remote(app.clone(), rec.id.clone(), client, info, session, tunnel);
    state.runtimes.write().await.insert(rec.id.clone(), Arc::new(rt));
    Ok(())
}

/// `secret` is the SSH password, or the key passphrase for key-based nodes.
#[tauri::command]
async fn connect_node(
    app: AppHandle,
    state: State<'_, AppState>,
    node_id: String,
    secret: Option<String>,
) -> Result<NodeView, String> {
    let rec = state
        .store
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&node_id)
        .cloned()
        .ok_or("unknown node")?;
    if state.runtimes.read().await.contains_key(&node_id) {
        return Ok(state.view(Some(&rec)).await);
    }
    let token = secrets::get_token(&rec.id)?;
    let target = SshTarget {
        host: rec.host.clone(),
        port: rec.ssh_port,
        username: rec.username.clone(),
        auth: ssh_auth(rec.key_path.as_ref(), secret)?,
    };
    let session = Arc::new(SshSession::connect(&target, &rec.host_key).await.map_err(|e| e.to_string())?);
    attach_remote(&app, &state, &rec, token, session).await?;
    Ok(state.view(Some(&rec)).await)
}

#[tauri::command]
async fn disconnect_node(state: State<'_, AppState>, node_id: String) -> Result<(), String> {
    if node_id == LOCAL_ID {
        return Err("the local node is always available".into());
    }
    state.runtimes.write().await.remove(&node_id);
    Ok(())
}

/// Forgets the node and its stored token. Does not uninstall anything from the server.
#[tauri::command]
async fn remove_node(state: State<'_, AppState>, node_id: String) -> Result<(), String> {
    if node_id == LOCAL_ID {
        return Err("cannot remove the local node".into());
    }
    state.runtimes.write().await.remove(&node_id);
    let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
    store.nodes.retain(|n| n.id != node_id);
    store.save()?;
    secrets::delete_token(&node_id);
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeployRequest {
    name: String,
    host: String,
    port: u16,
    username: String,
    password: Option<String>,
    key_path: Option<PathBuf>,
    passphrase: Option<String>,
    /// Fingerprint the user confirmed after `probe_host`.
    host_key: String,
    binary_path: PathBuf,
    media_roots: Vec<String>,
    install_ffmpeg: bool,
    /// Used only for this run when the SSH user needs `sudo`; never stored.
    sudo_password: Option<String>,
}

#[tauri::command]
async fn deploy_server(
    app: AppHandle,
    state: State<'_, AppState>,
    req: DeployRequest,
) -> Result<NodeView, String> {
    if !req.binary_path.is_file() {
        return Err(format!("daemon binary not found: {}", req.binary_path.display()));
    }
    let auth = match &req.key_path {
        Some(path) => SshAuth::KeyFile { path: path.clone(), passphrase: req.passphrase.clone().filter(|s| !s.is_empty()) },
        None => SshAuth::Password(req.password.clone().ok_or("password or key file required")?),
    };
    let target = SshTarget { host: req.host.trim().to_owned(), port: req.port, username: req.username.clone(), auth };
    let session = Arc::new(SshSession::connect(&target, &req.host_key).await.map_err(|e| e.to_string())?);

    let mut config = ProvisionConfig::new(req.binary_path.clone());
    config.install_ffmpeg = req.install_ffmpeg;
    config.sudo_password = req.sudo_password.clone().filter(|s| !s.is_empty()).map(Secret::new);
    if !req.media_roots.is_empty() {
        config.media_roots = req.media_roots.clone();
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let progress_app = app.clone();
    let forward = tokio::spawn(async move {
        while let Some(ev) = rx.recv().await {
            let _ = progress_app.emit("deploy-progress", ev);
        }
    });
    let outcome = provision(&session, &config, Some(tx)).await;
    let _ = forward.await;
    let outcome = outcome.map_err(|e| e.to_string())?;

    let rec = NodeRecord {
        id: uuid::Uuid::new_v4().to_string(),
        name: if req.name.trim().is_empty() { req.host.clone() } else { req.name.trim().to_owned() },
        host: target.host.clone(),
        ssh_port: req.port,
        username: req.username,
        key_path: req.key_path,
        host_key: req.host_key,
        remote_port: outcome.remote_port,
    };
    secrets::set_token(&rec.id, &outcome.token)?;
    {
        let mut store = state.store.lock().unwrap_or_else(|e| e.into_inner());
        store.nodes.push(rec.clone());
        store.save()?;
    }
    attach_remote(&app, &state, &rec, outcome.token, session).await?;
    Ok(state.view(Some(&rec)).await)
}

#[tauri::command]
async fn import_project(path: String) -> Result<encoder_import::ProjectSummary, String> {
    tokio::task::spawn_blocking(move || encoder_import::read_project(std::path::Path::new(&path)))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn match_masters(sequences: Vec<(u64, String)>, files: Vec<String>) -> Vec<encoder_import::MasterMatch> {
    encoder_import::match_masters(&sequences, &files)
}

/// Probes video files in `dir` on the node and suggests sequence masters whose frame size, rate
/// and length fit. Capped so a huge folder cannot stall the UI.
#[tauri::command]
async fn suggest_masters(
    state: State<'_, AppState>,
    node_id: String,
    dir: String,
    sequences: Vec<encoder_import::SeqSpec>,
    files: Vec<String>,
) -> Result<Vec<encoder_import::Suggestion>, String> {
    const MAX_FILES: usize = 40;
    const PARALLEL: usize = 8;
    let rt = state.runtime(&node_id).await?;
    let sep = if dir.contains('\\') && !dir.contains('/') { '\\' } else { '/' };
    let gate = Arc::new(tokio::sync::Semaphore::new(PARALLEL));
    let mut tasks = tokio::task::JoinSet::new();
    for name in files.into_iter().filter(|f| encoder_import::is_video(f)).take(MAX_FILES) {
        let (rt, gate) = (rt.clone(), gate.clone());
        let full = format!("{}{sep}{name}", dir.trim_end_matches(['/', '\\']));
        tasks.spawn(async move {
            let _permit = gate.acquire().await.ok()?;
            let p = rt.probe(&full).await?;
            Some(encoder_import::FileSpec { name, width: p.width, height: p.height, fps: p.fps, duration_secs: p.duration_secs })
        });
    }
    let mut specs = Vec::new();
    while let Some(done) = tasks.join_next().await {
        if let Ok(Some(spec)) = done {
            specs.push(spec);
        }
    }
    Ok(encoder_import::suggest_by_specs(&sequences, &specs))
}

#[tauri::command]
async fn list_dir(
    state: State<'_, AppState>,
    node_id: String,
    path: Option<String>,
) -> Result<encoder_core::fsutil::DirListing, String> {
    state.runtime(&node_id).await?.list_dir(path).await
}

#[tauri::command]
async fn list_jobs(state: State<'_, AppState>, node_id: String) -> Result<Vec<Job>, String> {
    state.runtime(&node_id).await?.list().await
}

#[tauri::command]
async fn submit_jobs(
    state: State<'_, AppState>,
    node_id: String,
    specs: Vec<JobSpec>,
) -> Result<Vec<Job>, String> {
    if specs.is_empty() {
        return Err("nothing to encode".into());
    }
    state.runtime(&node_id).await?.submit(specs).await
}

#[tauri::command]
async fn cancel_job(state: State<'_, AppState>, node_id: String, id: JobId) -> Result<(), String> {
    state.runtime(&node_id).await?.cancel(id).await
}

#[tauri::command]
async fn remove_job(state: State<'_, AppState>, node_id: String, id: JobId) -> Result<(), String> {
    state.runtime(&node_id).await?.remove(id).await
}

fn main() {
    tracing_subscriber::fmt().with_env_filter("warn").init();

    let app = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let config_dir = app.path().app_config_dir()?;
            let data_dir = app.path().app_data_dir()?;
            std::fs::create_dir_all(&data_dir)?;

            let mut runtimes = HashMap::new();
            let handle = app.handle().clone();
            tauri::async_runtime::block_on(async {
                let mut cfg = FfmpegConfig::default();
                let hw = HwCaps::detect(&cfg).await;
                let concurrency = hw.default_concurrency();
                cfg.threads_per_job = hw.default_threads_per_job(concurrency);
                let queue = JobQueue::start(
                    QueueConfig { max_concurrent: concurrency, snapshot_path: Some(data_dir.join("local-queue.json")) },
                    Arc::new(RoutingExecutor {
                        ffmpeg: FfmpegExecutor::new(cfg.clone(), hw.clone()),
                        filmcraft: encoder_filmcraft::FilmcraftExecutor::new(encoder_filmcraft::find_cli()),
                    }),
                )
                .await;
                let info = NodeInfo {
                    daemon_version: env!("CARGO_PKG_VERSION").into(),
                    max_concurrent: concurrency,
                    hardware: hw,
                };
                runtimes.insert(LOCAL_ID.to_owned(), Arc::new(Runtime::local(handle, queue, info, LOCAL_ID.into(), cfg)));
            });

            app.manage(AppState {
                store: std::sync::Mutex::new(NodeStore::load(config_dir.join("nodes.json"))),
                runtimes: RwLock::new(runtimes),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_nodes,
            probe_host,
            default_daemon_binary,
            connect_node,
            disconnect_node,
            remove_node,
            deploy_server,
            filmcraft_info,
            import_project,
            match_masters,
            suggest_masters,
            list_dir,
            list_jobs,
            submit_jobs,
            cancel_job,
            remove_job,
        ])
        .build(tauri::generate_context!())
        .expect("error while building EncoderCraft");

    app.run(|handle, event| {
        if let RunEvent::Exit = event {
            // Leaves running local jobs in the snapshot so they resume next launch.
            if let Some(state) = handle.try_state::<AppState>() {
                if let Ok(rts) = state.runtimes.try_read() {
                    if let Some(local) = rts.get(LOCAL_ID) {
                        local.shutdown();
                    }
                }
            }
        }
    });
}
