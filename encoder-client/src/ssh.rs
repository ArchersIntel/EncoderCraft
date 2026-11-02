//! Thin async SSH layer over `russh`: pinned host keys, remote exec, file upload, port tunnel.

use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use russh::{
    client::{self, Handle},
    keys::{load_secret_key, HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
    ChannelMsg, Disconnect,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Debug, thiserror::Error)]
pub enum SshError {
    #[error("connection to {host}:{port} timed out")]
    Timeout { host: String, port: u16 },
    #[error(
        "host key mismatch: expected {expected}, server presented {actual}. \
         Someone may be intercepting the connection, or the server was reinstalled."
    )]
    HostKeyMismatch { expected: String, actual: String },
    #[error("authentication failed for user '{0}'")]
    AuthFailed(String),
    #[error("could not read private key {path}: {source}")]
    Key { path: PathBuf, source: russh::keys::Error },
    #[error("command ended without an exit status: {0}")]
    NoExitStatus(String),
    #[error("ssh: {0}")]
    Ssh(#[from] russh::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub enum SshAuth {
    Password(String),
    KeyFile { path: PathBuf, passphrase: Option<String> },
}

impl fmt::Debug for SshAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Password(<redacted>)"),
            Self::KeyFile { path, .. } => write!(f, "KeyFile({})", path.display()),
        }
    }
}

#[derive(Debug)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub auth: SshAuth,
}

#[derive(Debug, Clone)]
pub struct ExecOutput {
    pub code: u32,
    pub stdout: String,
    pub stderr: String,
}

impl ExecOutput {
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

struct Handler {
    expected: Option<String>,
    observed: Arc<Mutex<Option<String>>>,
}

fn fingerprint(key: &PublicKeyOrCertificate) -> String {
    match key {
        PublicKeyOrCertificate::PublicKey { key, .. } => key.fingerprint(HashAlg::Sha256).to_string(),
        PublicKeyOrCertificate::Certificate(cert) => {
            cert.public_key().fingerprint(HashAlg::Sha256).to_string()
        }
    }
}

impl client::Handler for Handler {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let fp = fingerprint(key);
        let accept = self.expected.as_ref().is_none_or(|e| *e == fp);
        *self.observed.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp);
        Ok(accept)
    }
}

fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Package installs can be silent for minutes; rely on keepalives, not an idle timeout.
        inactivity_timeout: None,
        keepalive_interval: Some(Duration::from_secs(30)),
        ..Default::default()
    })
}

async fn open(
    host: &str,
    port: u16,
    expected: Option<String>,
) -> Result<(Handle<Handler>, Arc<Mutex<Option<String>>>), SshError> {
    let observed = Arc::new(Mutex::new(None));
    let handler = Handler { expected: expected.clone(), observed: observed.clone() };
    let connect = client::connect(config(), (host, port), handler);
    let result = tokio::time::timeout(CONNECT_TIMEOUT, connect)
        .await
        .map_err(|_| SshError::Timeout { host: host.into(), port })?;
    match result {
        Ok(handle) => Ok((handle, observed)),
        Err(e) => {
            let actual = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
            match (expected, actual) {
                (Some(expected), Some(actual)) if expected != actual => {
                    Err(SshError::HostKeyMismatch { expected, actual })
                }
                _ => Err(e.into()),
            }
        }
    }
}

/// Connects only to learn the server's host key fingerprint (`SHA256:...`), then disconnects
/// without authenticating. Show it to the user and have them confirm before calling
/// [`SshSession::connect`] with the same value.
pub async fn probe_host_key(host: &str, port: u16) -> Result<String, SshError> {
    let (handle, observed) = open(host, port, None).await?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    let fp = observed.lock().unwrap_or_else(|e| e.into_inner()).clone();
    fp.ok_or_else(|| russh::Error::Disconnect.into())
}

pub struct SshSession {
    handle: Handle<Handler>,
    pub username: String,
}

impl SshSession {
    /// `host_key` is the pinned fingerprint; the connection is refused if it differs.
    pub async fn connect(target: &SshTarget, host_key: &str) -> Result<Self, SshError> {
        let (mut handle, _) = open(&target.host, target.port, Some(host_key.to_owned())).await?;
        let user = target.username.clone();
        let result = match &target.auth {
            SshAuth::Password(pw) => handle.authenticate_password(user.clone(), pw.clone()).await?,
            SshAuth::KeyFile { path, passphrase } => {
                let key = load_secret_key(path, passphrase.as_deref())
                    .map_err(|source| SshError::Key { path: path.clone(), source })?;
                let hash = handle.best_supported_rsa_hash().await?.flatten();
                handle
                    .authenticate_publickey(user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                    .await?
            }
        };
        if !result.success() {
            return Err(SshError::AuthFailed(user));
        }
        Ok(Self { handle, username: target.username.clone() })
    }

    /// Runs `command` under the remote login shell, optionally feeding it `stdin`.
    /// Secrets belong in `stdin`, never in `command` (which is visible in `ps`).
    pub async fn exec(&self, command: &str, stdin: Option<&[u8]>) -> Result<ExecOutput, SshError> {
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, command).await?;
        if let Some(input) = stdin {
            channel.data(input).await?;
        }
        channel.eof().await?;
        collect(&mut channel, command).await
    }

    /// Streams a local file into `remote_path` (via `cat`, so no SFTP subsystem is required).
    /// `on_progress(sent, total)` is called as data is written.
    pub async fn upload(
        &self,
        local: &Path,
        remote_path: &str,
        mut on_progress: impl FnMut(u64, u64),
    ) -> Result<ExecOutput, SshError> {
        let mut file = tokio::fs::File::open(local).await?;
        let total = file.metadata().await?.len();
        let mut channel = self.handle.channel_open_session().await?;
        channel.exec(true, format!("cat > {}", crate::shell::quote(remote_path))).await?;

        let mut buf = vec![0u8; 32 * 1024];
        let mut sent = 0u64;
        loop {
            let n = file.read(&mut buf).await?;
            if n == 0 {
                break;
            }
            channel.data(&buf[..n]).await?;
            sent += n as u64;
            on_progress(sent, total);
        }
        channel.eof().await?;
        collect(&mut channel, "upload").await
    }

    /// Opens a connection from the remote host to `host:port` as seen from the remote side.
    pub async fn open_forward(
        &self,
        host: &str,
        port: u16,
    ) -> Result<russh::ChannelStream<client::Msg>, SshError> {
        let ch = self.handle.channel_open_direct_tcpip(host, u32::from(port), "127.0.0.1", 0).await?;
        Ok(ch.into_stream())
    }

    pub async fn close(&self) {
        let _ = self.handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }
}

async fn collect(
    channel: &mut russh::Channel<client::Msg>,
    what: &str,
) -> Result<ExecOutput, SshError> {
    let (mut stdout, mut stderr, mut code) = (Vec::new(), Vec::new(), None);
    while let Some(msg) = channel.wait().await {
        match msg {
            ChannelMsg::Data { data } => stdout.extend_from_slice(&data),
            ChannelMsg::ExtendedData { data, .. } => stderr.extend_from_slice(&data),
            ChannelMsg::ExitStatus { exit_status } => code = Some(exit_status),
            _ => {}
        }
    }
    let code = code.ok_or_else(|| SshError::NoExitStatus(what.to_owned()))?;
    Ok(ExecOutput {
        code,
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// A local TCP listener on loopback whose connections are forwarded over SSH to
/// `127.0.0.1:<remote_port>` on the server, so the daemon never needs a public port.
pub struct Tunnel {
    pub local_addr: std::net::SocketAddr,
    task: JoinHandle<()>,
}

impl Tunnel {
    pub async fn start(session: Arc<SshSession>, remote_port: u16) -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let local_addr = listener.local_addr()?;
        let task = tokio::spawn(async move {
            while let Ok((mut tcp, _)) = listener.accept().await {
                let session = session.clone();
                tokio::spawn(async move {
                    match session.open_forward("127.0.0.1", remote_port).await {
                        Ok(mut remote) => {
                            let _ = tokio::io::copy_bidirectional(&mut tcp, &mut remote).await;
                            let _ = remote.shutdown().await;
                        }
                        Err(e) => tracing::warn!("tunnel: could not open remote channel: {e}"),
                    }
                });
            }
        });
        Ok(Self { local_addr, task })
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.task.abort();
    }
}
