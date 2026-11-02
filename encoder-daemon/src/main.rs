//! `encraft-d`: headless EncoderCraft encoding node.

mod api;
mod paths;

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::{bail, Context};
use clap::Parser;
use encoder_core::{
    api::{NodeInfo, DEFAULT_PORT},
    ffmpeg::{FfmpegConfig, FfmpegExecutor, HwCaps},
    JobQueue, QueueConfig,
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "EncoderCraft headless encoding daemon")]
struct Args {
    /// Address to listen on. Keep this on loopback and reach it through an SSH tunnel.
    #[arg(long, env = "ENCRAFT_LISTEN", default_value_t = SocketAddr::from(([127, 0, 0, 1], DEFAULT_PORT)))]
    listen: SocketAddr,

    /// File containing the API bearer token (at least 32 characters).
    #[arg(long, env = "ENCRAFT_TOKEN_FILE")]
    token_file: PathBuf,

    /// Directory for the persisted job queue.
    #[arg(long, env = "ENCRAFT_STATE_DIR", default_value = "/var/lib/encodercraft")]
    state_dir: PathBuf,

    /// Directory jobs may read from and write to (repeatable). Mount SMB/NFS shares under one.
    #[arg(long = "media-root", required = true)]
    media_roots: Vec<PathBuf>,

    /// Concurrent jobs. Defaults to 3 with NVENC, otherwise a function of CPU threads.
    #[arg(long, env = "ENCRAFT_CONCURRENCY")]
    concurrency: Option<usize>,

    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,

    #[arg(long, default_value = "ffprobe")]
    ffprobe: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    let args = Args::parse();

    let token = std::fs::read_to_string(&args.token_file)
        .with_context(|| format!("reading token file {}", args.token_file.display()))?;
    let token = token.trim();
    if token.len() < 32 {
        bail!("token in {} is too short (need >= 32 characters)", args.token_file.display());
    }

    let roots = paths::MediaRoots::new(&args.media_roots).context("media root does not exist")?;

    let mut ffmpeg = FfmpegConfig { ffmpeg: args.ffmpeg, ffprobe: args.ffprobe, threads_per_job: 0 };
    let hw = HwCaps::detect(&ffmpeg).await;
    if hw.ffmpeg_version.is_none() {
        bail!("could not run `{}`; is FFmpeg installed?", ffmpeg.ffmpeg.display());
    }
    let concurrency = args.concurrency.unwrap_or_else(|| hw.default_concurrency()).max(1);
    ffmpeg.threads_per_job = hw.default_threads_per_job(concurrency);
    tracing::info!(?hw, concurrency, "hardware detected");

    tokio::fs::create_dir_all(&args.state_dir).await.context("creating state dir")?;
    let queue = JobQueue::start(
        QueueConfig {
            max_concurrent: concurrency,
            snapshot_path: Some(args.state_dir.join("queue.json")),
        },
        Arc::new(FfmpegExecutor::new(ffmpeg.clone(), hw.clone())),
    )
    .await;

    let state = api::AppState {
        queue: queue.clone(),
        roots,
        node: Arc::new(NodeInfo {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            max_concurrent: concurrency,
            hardware: hw,
        }),
        token: token.into(),
        ffmpeg,
    };

    if !args.listen.ip().is_loopback() {
        tracing::warn!(
            "listening on non-loopback address {}: traffic is unencrypted HTTP; \
             prefer 127.0.0.1 plus an SSH tunnel",
            args.listen
        );
    }
    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!("encraft-d {} listening on {}", env!("CARGO_PKG_VERSION"), args.listen);

    axum::serve(listener, api::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            tracing::info!("shutting down");
            queue.shutdown();
        })
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
