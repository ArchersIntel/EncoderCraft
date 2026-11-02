//! Remote provisioning: turn a bare Linux host into an EncoderCraft node.
//!
//! Steps: preflight checks, FFmpeg install, service user and directories, binary upload,
//! token + systemd unit, start and health check. Re-running it upgrades the binary and rotates
//! the API token.

use std::{path::PathBuf, sync::Arc, time::Duration};

use encoder_core::api::{API_PREFIX, DEFAULT_PORT};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::mpsc::UnboundedSender};

use crate::{
    shell::{is_safe_abs_path, is_safe_token, quote},
    ssh::{ExecOutput, SshError, SshSession},
};

const TOTAL_STEPS: u8 = 6;
const SERVICE_NAME: &str = "encraft-d";
const INSTALL_DIR: &str = "/opt/encodercraft";
const CONFIG_DIR: &str = "/etc/encodercraft";
const STATE_DIR: &str = "/var/lib/encodercraft";

/// A password that never prints itself.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone)]
pub struct ProvisionConfig {
    /// Password for `sudo`, used only when the SSH user is not root and has no passwordless sudo.
    /// Held in memory for the run; never written anywhere.
    pub sudo_password: Option<Secret>,
    /// Local path to the `encraft-d` binary built for the remote host (static musl recommended).
    pub daemon_binary: PathBuf,
    /// Expected `uname -m` of the remote host.
    pub arch: String,
    pub install_ffmpeg: bool,
    /// Unprivileged account the service runs as.
    pub service_user: String,
    /// Remote directories jobs may read and write; SMB/NFS shares should be mounted beneath one.
    pub media_roots: Vec<String>,
    pub port: u16,
}

impl ProvisionConfig {
    pub fn new(daemon_binary: PathBuf) -> Self {
        Self {
            sudo_password: None,
            daemon_binary,
            arch: "x86_64".into(),
            install_ffmpeg: true,
            service_user: "encraft".into(),
            media_roots: vec!["/mnt".into()],
            port: DEFAULT_PORT,
        }
    }

    fn validate(&self) -> Result<(), ProvisionError> {
        if !is_safe_token(&self.service_user) || self.service_user.contains('/') {
            return Err(ProvisionError::InvalidConfig(format!("bad service user '{}'", self.service_user)));
        }
        if self.media_roots.is_empty() {
            return Err(ProvisionError::InvalidConfig("at least one media root is required".into()));
        }
        if let Some(bad) = self.media_roots.iter().find(|p| !is_safe_abs_path(p)) {
            return Err(ProvisionError::InvalidConfig(format!(
                "media root '{bad}' must be an absolute path using only letters, digits and . _ - + /"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProvisionEvent {
    Step { index: u8, total: u8, name: &'static str },
    Log { message: String },
    Upload { sent: u64, total: u64 },
}

pub struct ProvisionOutcome {
    /// Bearer token for the daemon API. Shown once: store it in the OS keychain, not on disk.
    pub token: String,
    pub remote_port: u16,
    pub ffmpeg_version: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProvisionError {
    #[error(transparent)]
    Ssh(#[from] SshError),
    /// The user is not root and sudo wants a password; callers should prompt and retry.
    #[error("sudo needs a password for this user")]
    SudoPasswordRequired,
    #[error("invalid configuration: {0}")]
    InvalidConfig(String),
    #[error("{0}")]
    Preflight(String),
    #[error("step '{step}' failed (exit {code}): {detail}")]
    Command { step: &'static str, code: u32, detail: String },
    #[error("could not read daemon binary {path}: {source}")]
    Binary { path: PathBuf, source: std::io::Error },
    #[error("upload corrupted: local sha256 {local}, remote {remote}")]
    ChecksumMismatch { local: String, remote: String },
    #[error("service did not become healthy: {0}")]
    Unhealthy(String),
    #[error("could not generate token: {0}")]
    Random(String),
}

struct Reporter(Option<UnboundedSender<ProvisionEvent>>);

impl Reporter {
    fn send(&self, e: ProvisionEvent) {
        if let Some(tx) = &self.0 {
            let _ = tx.send(e);
        }
    }
    fn step(&self, index: u8, name: &'static str) {
        tracing::info!("[{index}/{TOTAL_STEPS}] {name}");
        self.send(ProvisionEvent::Step { index, total: TOTAL_STEPS, name });
    }
    fn log(&self, msg: impl Into<String>) {
        self.send(ProvisionEvent::Log { message: msg.into() });
    }
}

/// Runs everything. `events` receives progress for a UI; pass `None` to ignore it.
pub async fn provision(
    session: &Arc<SshSession>,
    config: &ProvisionConfig,
    events: Option<UnboundedSender<ProvisionEvent>>,
) -> Result<ProvisionOutcome, ProvisionError> {
    config.validate()?;
    let rep = Reporter(events);
    let remote = Remote { session: session.as_ref(), privilege: Privilege::SudoNoPassword };

    // 1. Preflight
    rep.step(1, "Checking remote host");
    let remote = preflight(remote, config, &rep).await?;

    // 2. FFmpeg
    rep.step(2, "Checking FFmpeg");
    let ffmpeg_version = ensure_ffmpeg(&remote, config, &rep).await?;

    // 3. User and directories
    rep.step(3, "Creating service user and directories");
    create_user_and_dirs(&remote, config).await?;

    // 4. Upload
    rep.step(4, "Uploading encraft-d");
    upload_binary(&remote, config, &rep).await?;

    // 5. Token and unit
    rep.step(5, "Writing configuration and systemd unit");
    let token = random_token()?;
    remote
        .sudo_with_file(
            &format!(
                "umask 027 && cat {{F}} > {CONFIG_DIR}/token && chown root:{u} {CONFIG_DIR}/token && chmod 640 {CONFIG_DIR}/token",
                u = config.service_user
            ),
            token.as_bytes(),
            "write token",
        )
        .await?;
    remote
        .sudo_with_file(
            &format!("cat {{F}} > /etc/systemd/system/{SERVICE_NAME}.service && chmod 644 /etc/systemd/system/{SERVICE_NAME}.service"),
            render_unit(config).as_bytes(),
            "write systemd unit",
        )
        .await?;

    // 6. Start
    rep.step(6, "Starting service");
    remote.sudo("systemctl daemon-reload", "systemd reload").await?;
    remote.sudo(&format!("systemctl enable {SERVICE_NAME}"), "enable service").await?;
    remote.sudo(&format!("systemctl restart {SERVICE_NAME}"), "start service").await?;
    wait_healthy(&remote, config, &rep).await?;

    Ok(ProvisionOutcome { token, remote_port: config.port, ffmpeg_version })
}

/// How privileged commands are run on the remote host.
#[derive(Clone, Copy)]
enum Privilege<'a> {
    Root,
    /// Passwordless sudo.
    SudoNoPassword,
    /// `sudo -S`, with the password fed on stdin. It lives only in memory for this run.
    SudoPassword(&'a str),
}

/// Handle that knows whether commands need `sudo`.
#[derive(Clone, Copy)]
struct Remote<'a> {
    session: &'a SshSession,
    privilege: Privilege<'a>,
}

impl Remote<'_> {
    fn wrap(&self, script: &str) -> String {
        match self.privilege {
            Privilege::Root => format!("sh -c {}", quote(script)),
            Privilege::SudoNoPassword => format!("sudo -n sh -c {}", quote(script)),
            // `-k` ignores cached credentials, so sudo always consumes exactly one stdin line
            // (the password) and never lets it leak into the command's own input.
            Privilege::SudoPassword(_) => format!("sudo -S -p '' -k sh -c {}", quote(script)),
        }
    }

    async fn run(&self, script: &str) -> Result<ExecOutput, SshError> {
        self.session.exec(&format!("sh -c {}", quote(script)), None).await
    }

    async fn sudo_out(&self, script: &str) -> Result<ExecOutput, SshError> {
        let cmd = self.wrap(script);
        match self.privilege {
            Privilege::SudoPassword(pw) => self.session.exec(&cmd, Some(format!("{pw}\n").as_bytes())).await,
            _ => self.session.exec(&cmd, None).await,
        }
    }

    async fn sudo(&self, script: &str, step: &'static str) -> Result<ExecOutput, ProvisionError> {
        check(self.sudo_out(script).await?, step)
    }

    /// Runs `script` as root with `payload` available as the file `{F}`. The payload goes through
    /// a private temp file because sudo's stdin is reserved for the password.
    async fn sudo_with_file(&self, script: &str, payload: &[u8], step: &'static str) -> Result<(), ProvisionError> {
        let tmp = check(self.run("umask 077; mktemp /tmp/encraft-cfg.XXXXXX").await?, "mktemp")?;
        let tmp = tmp.stdout.trim().to_owned();
        let result = async {
            let write = format!("sh -c {}", quote(&format!("cat > {}", quote(&tmp))));
            check(self.session.exec(&write, Some(payload)).await?, step)?;
            self.sudo(&script.replace("{F}", &quote(&tmp)), step).await?;
            Ok(())
        }
        .await;
        let _ = self.run(&format!("rm -f {}", quote(&tmp))).await;
        result
    }
}

fn check(out: ExecOutput, step: &'static str) -> Result<ExecOutput, ProvisionError> {
    if out.success() {
        return Ok(out);
    }
    let mut detail = out.stderr.trim().to_owned();
    if detail.is_empty() {
        detail = out.stdout.trim().to_owned();
    }
    // Keep error text bounded; apt can be chatty.
    let tail: Vec<&str> = detail.lines().rev().take(8).collect();
    let detail = tail.into_iter().rev().collect::<Vec<_>>().join("\n");
    Err(ProvisionError::Command { step, code: out.code, detail })
}

async fn preflight<'a>(
    remote: Remote<'a>,
    config: &'a ProvisionConfig,
    rep: &Reporter,
) -> Result<Remote<'a>, ProvisionError> {
    let uname = check(remote.run("uname -sm").await?, "uname")?;
    let uname = uname.stdout.trim().to_owned();
    rep.log(format!("Remote: {uname}"));
    let (os, arch) = uname.split_once(' ').unwrap_or((&uname, ""));
    if os != "Linux" {
        return Err(ProvisionError::Preflight(format!("remote OS is {os}; only Linux is supported")));
    }
    if arch != config.arch {
        return Err(ProvisionError::Preflight(format!(
            "remote architecture is {arch} but the daemon binary is built for {}",
            config.arch
        )));
    }
    if !remote.run("test -d /run/systemd/system").await?.success() {
        return Err(ProvisionError::Preflight("remote host is not running systemd".into()));
    }

    let uid = check(remote.run("id -u").await?, "id")?;
    if uid.stdout.trim() == "0" {
        return Ok(Remote { privilege: Privilege::Root, ..remote });
    }
    let nopass = Remote { privilege: Privilege::SudoNoPassword, ..remote };
    if nopass.sudo_out("true").await?.success() {
        rep.log("Using passwordless sudo");
        return Ok(nopass);
    }
    let Some(pw) = config.sudo_password.as_ref().map(|s| s.expose()).filter(|s| !s.is_empty()) else {
        return Err(ProvisionError::SudoPasswordRequired);
    };
    let with_pw = Remote { privilege: Privilege::SudoPassword(pw), ..remote };
    if !with_pw.sudo_out("true").await?.success() {
        return Err(ProvisionError::Preflight(
            "sudo rejected the password (or this user is not allowed to use sudo)".into(),
        ));
    }
    rep.log("Using sudo with the supplied password (not stored)");
    Ok(with_pw)
}

async fn ensure_ffmpeg(
    remote: &Remote<'_>,
    config: &ProvisionConfig,
    rep: &Reporter,
) -> Result<String, ProvisionError> {
    let have = "command -v ffmpeg >/dev/null && command -v ffprobe >/dev/null";
    if !remote.run(have).await?.success() {
        if !config.install_ffmpeg {
            return Err(ProvisionError::Preflight("FFmpeg is not installed on the remote host".into()));
        }
        rep.log("FFmpeg not found; installing from the distribution's packages");
        let pm = remote
            .run(
                "for pm in apt-get dnf yum pacman zypper; do command -v $pm >/dev/null && echo $pm && break; done",
            )
            .await?
            .stdout;
        let install = match pm.trim() {
            "apt-get" => "export DEBIAN_FRONTEND=noninteractive; apt-get update -qq && apt-get install -y --no-install-recommends ffmpeg",
            "dnf" => "dnf install -y ffmpeg || dnf install -y ffmpeg-free",
            "yum" => "yum install -y ffmpeg",
            "pacman" => "pacman -S --noconfirm ffmpeg",
            "zypper" => "zypper --non-interactive install ffmpeg",
            _ => {
                return Err(ProvisionError::Preflight(
                    "no supported package manager found; install FFmpeg manually and retry".into(),
                ))
            }
        };
        remote.sudo(install, "install ffmpeg").await.map_err(|e| match e {
            ProvisionError::Command { detail, .. } if pm.trim() == "dnf" || pm.trim() == "yum" => {
                ProvisionError::Preflight(format!(
                    "could not install ffmpeg ({detail}). On RHEL-family systems enable RPM Fusion \
                     (or another repository that ships ffmpeg) and retry."
                ))
            }
            other => other,
        })?;
        if !remote.run(have).await?.success() {
            return Err(ProvisionError::Preflight("ffmpeg still missing after installation".into()));
        }
    }
    let v = check(remote.run("ffmpeg -version | head -n1").await?, "ffmpeg version")?;
    let v = v.stdout.trim().to_owned();
    rep.log(v.clone());
    Ok(v)
}

async fn create_user_and_dirs(remote: &Remote<'_>, config: &ProvisionConfig) -> Result<(), ProvisionError> {
    let u = &config.service_user;
    let mut script = format!(
        "id -u {u} >/dev/null 2>&1 || useradd --system --user-group --no-create-home \
         --home-dir {STATE_DIR} --shell \"$(command -v nologin || echo /bin/false)\" {u}; \
         install -d -m 755 {INSTALL_DIR}; \
         install -d -m 750 -o {u} -g {u} {STATE_DIR}; \
         install -d -m 750 -o root -g {u} {CONFIG_DIR}"
    );
    for root in &config.media_roots {
        script.push_str(&format!("; mkdir -p {}", quote(root)));
    }
    remote.sudo(&script, "create user and directories").await?;
    Ok(())
}

async fn upload_binary(
    remote: &Remote<'_>,
    config: &ProvisionConfig,
    rep: &Reporter,
) -> Result<(), ProvisionError> {
    let bytes = tokio::fs::read(&config.daemon_binary)
        .await
        .map_err(|source| ProvisionError::Binary { path: config.daemon_binary.clone(), source })?;
    let local_sum = hex(&Sha256::digest(&bytes));
    drop(bytes);

    let tmp = check(remote.run("mktemp /tmp/encraft-d.XXXXXX").await?, "mktemp")?;
    let tmp = tmp.stdout.trim().to_owned();

    let result = async {
        let out = remote
            .session
            .upload(&config.daemon_binary, &tmp, |sent, total| {
                rep.send(ProvisionEvent::Upload { sent, total })
            })
            .await?;
        check(out, "upload")?;

        let sum = check(remote.run(&format!("sha256sum {}", quote(&tmp))).await?, "sha256sum")?;
        let remote_sum = sum.stdout.split_whitespace().next().unwrap_or("").to_owned();
        if remote_sum != local_sum {
            return Err(ProvisionError::ChecksumMismatch { local: local_sum.clone(), remote: remote_sum });
        }
        // `install` replaces the file by unlinking first, so this works while the old binary runs.
        remote
            .sudo(
                &format!("install -m 755 -o root -g root {} {INSTALL_DIR}/{SERVICE_NAME}", quote(&tmp)),
                "install binary",
            )
            .await?;
        Ok(())
    }
    .await;

    let _ = remote.run(&format!("rm -f {}", quote(&tmp))).await;
    result
}

/// Hardened unit. `ProtectSystem=strict` makes the filesystem read-only except where listed, so
/// every media root must appear in `ReadWritePaths`.
pub fn render_unit(config: &ProvisionConfig) -> String {
    let roots_args: String = config.media_roots.iter().map(|r| format!(" --media-root {r}")).collect();
    let rw: String = config.media_roots.iter().map(|r| format!(" {r}")).collect();
    let u = &config.service_user;
    format!(
        "[Unit]\n\
         Description=EncoderCraft encoding daemon\n\
         After=network-online.target remote-fs.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         Type=simple\n\
         User={u}\n\
         Group={u}\n\
         ExecStart={INSTALL_DIR}/{SERVICE_NAME} --listen 127.0.0.1:{port} --token-file {CONFIG_DIR}/token --state-dir {STATE_DIR}{roots_args}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         NoNewPrivileges=true\n\
         PrivateTmp=true\n\
         ProtectSystem=strict\n\
         ProtectKernelTunables=true\n\
         ProtectControlGroups=true\n\
         ReadWritePaths={STATE_DIR}{rw}\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        port = config.port,
    )
}

async fn wait_healthy(
    remote: &Remote<'_>,
    config: &ProvisionConfig,
    rep: &Reporter,
) -> Result<(), ProvisionError> {
    let mut last = String::from("service never reported active");
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        let active = remote.sudo_out(&format!("systemctl is-active {SERVICE_NAME}")).await?;
        if active.stdout.trim() == "active" {
            match health_check(remote.session, config.port).await {
                Ok(()) => {
                    rep.log("Daemon is up and answering on 127.0.0.1 (remote side)");
                    return Ok(());
                }
                Err(e) => last = format!("service active but health check failed: {e}"),
            }
        } else {
            last = format!("systemd state: {}", active.stdout.trim());
        }
    }
    let logs = remote
        .sudo_out(&format!("journalctl -u {SERVICE_NAME} -n 25 --no-pager"))
        .await
        .map(|o| o.stdout)
        .unwrap_or_default();
    Err(ProvisionError::Unhealthy(format!("{last}\n{logs}")))
}

/// Minimal HTTP/1.0 GET over an SSH-forwarded channel, so no HTTP client is needed here.
pub async fn health_check(session: &SshSession, port: u16) -> Result<(), SshError> {
    use tokio::io::AsyncReadExt;
    let mut stream = session.open_forward("127.0.0.1", port).await?;
    let req = format!("GET {API_PREFIX}/health HTTP/1.0\r\nHost: localhost\r\n\r\n");
    stream.write_all(req.as_bytes()).await?;
    let mut resp = Vec::new();
    stream.read_to_end(&mut resp).await?;
    let resp = String::from_utf8_lossy(&resp);
    if resp.starts_with("HTTP/1.0 200") || resp.starts_with("HTTP/1.1 200") {
        Ok(())
    } else {
        Err(SshError::Io(std::io::Error::other(format!(
            "unexpected response: {}",
            resp.lines().next().unwrap_or("")
        ))))
    }
}

fn random_token() -> Result<String, ProvisionError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| ProvisionError::Random(e.to_string()))?;
    Ok(hex(&bytes))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_password_never_appears_in_debug_output() {
        let mut cfg = ProvisionConfig::new("encraft-d".into());
        cfg.sudo_password = Some(Secret::new("hunter2"));
        assert!(!format!("{cfg:?}").contains("hunter2"));
    }

    #[test]
    fn unit_lists_every_media_root_as_writable() {
        let mut cfg = ProvisionConfig::new("encraft-d".into());
        cfg.media_roots = vec!["/mnt".into(), "/srv/media".into()];
        let unit = render_unit(&cfg);
        assert!(unit.contains("--listen 127.0.0.1:7878"));
        assert!(unit.contains("--media-root /mnt --media-root /srv/media"));
        assert!(unit.contains("ReadWritePaths=/var/lib/encodercraft /mnt /srv/media"));
        assert!(unit.contains("NoNewPrivileges=true"));
    }

    #[test]
    fn config_rejects_injection() {
        let mut cfg = ProvisionConfig::new("x".into());
        cfg.media_roots = vec!["/mnt; rm -rf /".into()];
        assert!(cfg.validate().is_err());
        let mut cfg = ProvisionConfig::new("x".into());
        cfg.service_user = "root; id".into();
        assert!(cfg.validate().is_err());
        assert!(ProvisionConfig::new("x".into()).validate().is_ok());
    }

    #[test]
    fn token_is_64_hex_chars_and_unique() {
        let (a, b) = (random_token().unwrap(), random_token().unwrap());
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
