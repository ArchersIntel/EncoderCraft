//! Developer CLI for the provisioning flow the GUI wizard will drive.

use std::{
    io::{self, Write},
    path::PathBuf,
    sync::Arc,
};

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use encoder_client::{
    provision::{provision, ProvisionConfig, ProvisionEvent, Secret},
    ssh::{probe_host_key, SshAuth, SshSession, SshTarget},
};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(about = "EncoderCraft client tools")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install FFmpeg, upload encraft-d and enable it as a systemd service on a Linux host.
    ///
    /// The SSH password is read from $ENCRAFT_SSH_PASSWORD (never from argv); a key passphrase
    /// from $ENCRAFT_SSH_KEY_PASSPHRASE.
    Deploy {
        host: String,
        #[arg(long, default_value_t = 22)]
        port: u16,
        #[arg(long, short)]
        user: String,
        #[arg(long, short = 'k')]
        key: Option<PathBuf>,
        /// Local encraft-d binary built for the remote architecture.
        #[arg(long)]
        binary: PathBuf,
        /// Pinned host key fingerprint (SHA256:...). If omitted you are asked to confirm it.
        #[arg(long)]
        host_key: Option<String>,
        /// Remote directory jobs may use (repeatable). Default: /mnt
        #[arg(long = "media-root")]
        media_roots: Vec<String>,
        #[arg(long)]
        no_install_ffmpeg: bool,
        #[arg(long, default_value = "x86_64")]
        arch: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()))
        .init();

    match Cli::parse().command {
        Cmd::Deploy { host, port, user, key, binary, host_key, media_roots, no_install_ffmpeg, arch } => {
            let host_key = match host_key {
                Some(k) => k,
                None => {
                    let fp = probe_host_key(&host, port).await?;
                    println!("{host}:{port} presented host key {fp}");
                    print!("Trust this key and continue? [y/N] ");
                    io::stdout().flush()?;
                    let mut answer = String::new();
                    io::stdin().read_line(&mut answer)?;
                    if !answer.trim().eq_ignore_ascii_case("y") {
                        bail!("aborted: host key not trusted");
                    }
                    fp
                }
            };

            let auth = match key {
                Some(path) => SshAuth::KeyFile { path, passphrase: std::env::var("ENCRAFT_SSH_KEY_PASSPHRASE").ok() },
                None => SshAuth::Password(
                    std::env::var("ENCRAFT_SSH_PASSWORD")
                        .context("set ENCRAFT_SSH_PASSWORD, or pass --key")?,
                ),
            };
            let target = SshTarget { host, port, username: user, auth };
            let session = Arc::new(SshSession::connect(&target, &host_key).await?);

            let mut config = ProvisionConfig::new(binary);
            config.arch = arch;
            config.install_ffmpeg = !no_install_ffmpeg;
            config.sudo_password = std::env::var("ENCRAFT_SUDO_PASSWORD").ok().map(Secret::new);
            if !media_roots.is_empty() {
                config.media_roots = media_roots;
            }

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let printer = tokio::spawn(async move {
                while let Some(ev) = rx.recv().await {
                    match ev {
                        ProvisionEvent::Step { index, total, name } => println!("[{index}/{total}] {name}"),
                        ProvisionEvent::Log { message: m } => println!("      {m}"),
                        ProvisionEvent::Upload { sent, total } if sent == total => {
                            println!("      uploaded {} MiB", total / (1024 * 1024))
                        }
                        ProvisionEvent::Upload { .. } => {}
                    }
                }
            });
            let outcome = provision(&session, &config, Some(tx)).await;
            let _ = printer.await;
            session.close().await;
            let outcome = outcome?;

            println!("\nDeployed. The daemon listens on 127.0.0.1:{} on the server (not exposed).", outcome.remote_port);
            println!("API token (shown once, keep it secret): {}", outcome.token);
        }
    }
    Ok(())
}
