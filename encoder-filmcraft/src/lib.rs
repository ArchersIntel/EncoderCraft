//! Renders FilmCraft project sequences by driving FilmCraft's own headless CLI
//! (`filmcraft-cli mcp`, a JSON-RPC session over stdio). No project data is read or written by
//! EncoderCraft; FilmCraft opens the project (read-only), relinks moved media, and renders.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::{Duration, Instant},
};

use encoder_core::{
    executor::{BoxFuture, CancellationToken, ExecError, Executor, ProgressFn},
    ffmpeg::{no_window, temp_output_path},
    job::{FilmcraftSource, HwAccel, JobSpec, Progress, VideoCodec},
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Semaphore,
};

const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_EVERY: Duration = Duration::from_millis(500);

/// Where FilmCraft's CLI lives: `$FILMCRAFT_CLI`, the default install folder, then `PATH`.
pub fn find_cli() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("FILMCRAFT_CLI").map(PathBuf::from).filter(|p| p.is_file()) {
        return Some(p);
    }
    let mut candidates = Vec::new();
    for var in ["ProgramFiles", "ProgramW6432", "LOCALAPPDATA"] {
        if let Some(base) = std::env::var_os(var) {
            let base = PathBuf::from(base);
            candidates.push(base.join("ArtCraft").join("FilmCraft").join("filmcraft-cli.exe"));
            candidates.push(base.join("Programs").join("ArtCraft").join("FilmCraft").join("filmcraft-cli.exe"));
        }
    }
    candidates.push("/usr/local/bin/filmcraft-cli".into());
    candidates.push("/usr/bin/filmcraft-cli".into());
    if let Some(found) = candidates.into_iter().find(|p| p.is_file()) {
        return Some(found);
    }
    let exe = if cfg!(windows) { "filmcraft-cli.exe" } else { "filmcraft-cli" };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|d| d.join(exe))
        .find(|p| p.is_file())
}

/// `filmcraft-cli --version` (e.g. `filmcraft-cli 0.16.0`).
pub async fn cli_version(cli: &Path) -> Option<String> {
    let out = no_window(&mut Command::new(cli)).arg("--version").stdin(Stdio::null()).output().await.ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// A headless FilmCraft session with one project open. The process is killed on drop.
pub struct Session {
    _child: Child,
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    next_id: u64,
}

impl Session {
    pub async fn start(cli: &Path, project: &Path) -> Result<Self, String> {
        let mut child = no_window(&mut Command::new(cli))
            .arg("--project")
            .arg(project)
            .arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start FilmCraft ({}): {e}", cli.display()))?;
        let stdin = child.stdin.take().expect("piped");
        let lines = BufReader::new(child.stdout.take().expect("piped")).lines();
        let mut s = Self { _child: child, stdin, lines, next_id: 0 };
        s.rpc(
            "initialize",
            json!({"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "EncoderCraft", "version": env!("CARGO_PKG_VERSION")}}),
        )
        .await?;
        s.send(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).await?;
        Ok(s)
    }

    async fn send(&mut self, v: &Value) -> Result<(), String> {
        let mut line = v.to_string();
        line.push('\n');
        self.stdin.write_all(line.as_bytes()).await.map_err(|e| format!("FilmCraft closed the session: {e}"))?;
        self.stdin.flush().await.map_err(|e| e.to_string())
    }

    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})).await?;
        let wait = async {
            loop {
                let line = self.lines.next_line().await.map_err(|e| e.to_string())?.ok_or("FilmCraft exited unexpectedly")?;
                let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
                if v.get("id").and_then(Value::as_u64) == Some(id) {
                    return Ok::<Value, String>(v);
                }
            }
        };
        let v = tokio::time::timeout(CALL_TIMEOUT, wait).await.map_err(|_| "FilmCraft did not answer in time".to_owned())??;
        if let Some(e) = v.get("error") {
            return Err(e.get("message").and_then(Value::as_str).unwrap_or("FilmCraft error").to_owned());
        }
        Ok(v["result"].clone())
    }

    /// Runs one engine command and returns its JSON result.
    pub async fn command(&mut self, id: &str, params: Value) -> Result<Value, String> {
        let r = self.rpc("tools/call", json!({"name": "command_run", "arguments": {"id": id, "params": params}})).await?;
        let text = r["content"][0]["text"].as_str().unwrap_or("").to_owned();
        if r["isError"].as_bool() == Some(true) {
            return Err(if text.is_empty() { format!("{id} failed") } else { text });
        }
        Ok(serde_json::from_str(&text).unwrap_or(Value::String(text)))
    }
}

/// Output frame size and rate of a sequence, as resolved by FilmCraft.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Dims {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// FilmCraft has no CRF mode for H.265, so HEVC gets a VBR bitrate derived from the quality
/// setting: bits per pixel halve every +5 quality steps, anchored at ~0.095 bpp (about 6 Mbps for
/// 1080p30) for quality 26. Clamped to a sane range.
pub fn hevc_bitrate_kbps(d: &Dims, quality: u8) -> u32 {
    let bpp = 0.095 * 2f64.powf((26.0 - f64::from(quality)) / 5.0);
    let kbps = f64::from(d.width) * f64::from(d.height) * d.fps.max(1.0) * bpp / 1000.0;
    kbps.round().clamp(1_000.0, 120_000.0) as u32
}

/// Codec check that does not need FilmCraft running, so bad requests fail fast.
pub fn check_codec(spec: &JobSpec) -> Result<(), String> {
    match spec.video {
        VideoCodec::H264 | VideoCodec::Hevc => Ok(()),
        VideoCodec::Av1 => Err("AV1 is not available for project sequences; choose H.264 or HEVC".into()),
        VideoCodec::Copy => Err("'Copy' only applies to media files, not project sequences".into()),
    }
}

/// `file.exportMedia` parameters for a job. HEVC needs `dims` (see [`hevc_bitrate_kbps`]).
pub fn export_params(spec: &JobSpec, path: &Path, sequence: u64, dims: Option<&Dims>) -> Result<Value, String> {
    check_codec(spec)?;
    let mut p = json!({
        "path": path,
        "sequence": sequence,
        "hardwareEncoding": if spec.accel == HwAccel::Cpu { "off" } else { "auto" },
        "audio": true,
        "wait": false,
    });
    let o = p.as_object_mut().expect("object");
    match spec.video {
        VideoCodec::H264 => {
            o.insert("format".into(), "h264".into());
            o.insert("bitrateMode".into(), "crf".into());
            o.insert("crf".into(), spec.quality.min(51).into());
        }
        _ => {
            let d = dims.ok_or("sequence size is needed to pick an HEVC bitrate")?;
            o.insert("format".into(), "hevc".into());
            o.insert("bitrateMode".into(), "vbr1Pass".into());
            o.insert("bitrateKbps".into(), hevc_bitrate_kbps(d, spec.quality).into());
        }
    }
    Ok(p)
}

/// Reads the sequence's output size and frame rate from `export.resolve`.
fn parse_dims(resolved: &Value) -> Option<Dims> {
    let o = &resolved["output"];
    let fps = o["fps"].as_f64().or_else(|| o["fps"].as_str().and_then(|s| s.parse().ok()))?;
    Some(Dims { width: o["width"].as_u64()? as u32, height: o["height"].as_u64()? as u32, fps })
}
/// Renders `FilmcraftSource` jobs. Heavy (it composites every frame), so one at a time.
pub struct FilmcraftExecutor {
    cli: Option<PathBuf>,
    gate: Arc<Semaphore>,
}

impl FilmcraftExecutor {
    pub fn new(cli: Option<PathBuf>) -> Self {
        Self { cli, gate: Arc::new(Semaphore::new(1)) }
    }

    async fn run(&self, spec: JobSpec, on_progress: ProgressFn, cancel: CancellationToken) -> Result<(), ExecError> {
        let fail = |m: String| ExecError::Failed(m);
        let src = spec.filmcraft.clone().ok_or_else(|| fail("not a project sequence".into()))?;
        let cli = self.cli.clone().ok_or_else(|| fail("FilmCraft is not installed (filmcraft-cli not found)".into()))?;
        if !src.project.is_file() {
            return Err(fail(format!("project not found: {}", src.project.display())));
        }
        if !spec.overwrite && spec.output.exists() {
            return Err(fail(format!("output already exists: {}", spec.output.display())));
        }
        if let Some(dir) = spec.output.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| fail(format!("cannot create {}: {e}", dir.display())))?;
        }
        let tmp = temp_output_path(&spec.output);
        check_codec(&spec).map_err(fail)?;

        // One render at a time. The queue already counts this job as processing, so say that it
        // is really waiting for the renderer.
        if self.gate.available_permits() == 0 {
            on_progress(Progress { waiting: true, ..Default::default() });
        }
        let _permit = tokio::select! {
            p = self.gate.acquire() => p.map_err(|e| fail(e.to_string()))?,
            _ = cancel.cancelled() => return Err(ExecError::Cancelled),
        };
        on_progress(Progress::default());

        let outcome = self.render(&cli, &src, &spec, &tmp, &on_progress, &cancel).await;
        match outcome {
            Ok(()) => tokio::fs::rename(&tmp, &spec.output)
                .await
                .map_err(|e| fail(format!("finalizing {}: {e}", spec.output.display()))),
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                Err(e)
            }
        }
    }

    async fn render(
        &self,
        cli: &Path,
        src: &FilmcraftSource,
        spec: &JobSpec,
        tmp: &Path,
        on_progress: &ProgressFn,
        cancel: &CancellationToken,
    ) -> Result<(), ExecError> {
        let fail = |m: String| ExecError::Failed(m);
        let project = src.project.as_path();
        let mut s = Session::start(cli, project).await.map_err(fail)?;

        // Projects often reference media by an old drive letter; relink from the project's folder.
        if let Some(dir) = project.parent() {
            if let Err(e) = s.command("media.autoRelink", json!({"folder": dir})).await {
                tracing::warn!("relink failed (continuing): {e}");
            }
        }
        let dims = if spec.video == VideoCodec::Hevc {
            let resolved = s
                .command("export.resolve", json!({"format": "hevc", "sequence": src.sequence, "path": tmp}))
                .await
                .map_err(fail)?;
            Some(parse_dims(&resolved).ok_or_else(|| fail("could not read the sequence size from FilmCraft".into()))?)
        } else {
            None
        };
        let params = export_params(spec, tmp, src.sequence, dims.as_ref()).map_err(fail)?;
        let started = s.command("file.exportMedia", params).await.map_err(fail)?;
        let job = started["job"].as_u64().ok_or_else(|| fail("FilmCraft did not start the export".into()))?;
        let t0 = Instant::now();

        loop {
            tokio::select! {
                _ = tokio::time::sleep(POLL_EVERY) => {}
                _ = cancel.cancelled() => {
                    let _ = s.command("jobs.cancel", json!({"job": job})).await;
                    return Err(ExecError::Cancelled);
                }
            }
            let list = s.command("jobs.list", json!({})).await.map_err(fail)?;
            let Some(j) = list.as_array().and_then(|a| a.iter().find(|j| j["id"].as_u64() == Some(job))) else {
                return Err(fail("FilmCraft lost track of the export job".into()));
            };
            let done = j["done"].as_u64().unwrap_or(0);
            if j["finished"].as_bool() == Some(true) {
                return finish(j);
            }
            let secs = t0.elapsed().as_secs_f32().max(0.001);
            on_progress(Progress {
                percent: Some((j["progress"].as_f64().unwrap_or(0.0) * 100.0).clamp(0.0, 99.9) as f32),
                frame: done,
                fps: (done > 0).then(|| done as f32 / secs),
                speed: None,
                out_time_ms: 0,
                eta_secs: j["etaSeconds"].as_f64().map(|e| e as f32),
                waiting: false,
            });
        }
    }
}

/// Interprets a finished `jobs.list` entry.
fn finish(j: &Value) -> Result<(), ExecError> {
    let offline: Vec<String> = j["offlineMedia"]
        .as_array()
        .map(|a| a.iter().map(|m| m.as_str().map(str::to_owned).unwrap_or_else(|| m.to_string())).collect())
        .unwrap_or_default();
    if !offline.is_empty() {
        let shown: Vec<_> = offline.iter().take(3).cloned().collect();
        let more = if offline.len() > 3 { format!(" and {} more", offline.len() - 3) } else { String::new() };
        return Err(ExecError::Failed(format!("{} media file(s) are offline: {}{more}", offline.len(), shown.join(", "))));
    }
    let result = &j["result"];
    if let Some(e) = result.get("error").and_then(Value::as_str) {
        return Err(ExecError::Failed(e.to_owned()));
    }
    if result.is_null() || result.get("bytes").is_none() {
        let status = j["status"].as_str().filter(|s| !s.is_empty()).unwrap_or("export did not produce a file");
        return Err(ExecError::Failed(status.to_owned()));
    }
    Ok(())
}

impl Executor for FilmcraftExecutor {
    fn execute(&self, spec: JobSpec, on_progress: ProgressFn, cancel: CancellationToken) -> BoxFuture<'_, Result<(), ExecError>> {
        Box::pin(self.run(spec, on_progress, cancel))
    }
}

#[cfg(test)]
mod tests {
    use encoder_core::job::AudioCodec;

    use super::*;

    fn spec(video: VideoCodec, accel: HwAccel, quality: u8) -> JobSpec {
        JobSpec {
            input: "p.fcproj".into(),
            output: "out.mp4".into(),
            video,
            accel,
            quality,
            audio: AudioCodec::default(),
            overwrite: false,
            filmcraft: Some(FilmcraftSource { project: "p.fcproj".into(), sequence: 7, sequence_name: "S".into() }),
        }
    }

    #[test]
    fn maps_codec_quality_and_hardware() {
        let d = Dims { width: 1920, height: 1080, fps: 29.97 };
        let p = export_params(&spec(VideoCodec::H264, HwAccel::Auto, 23), Path::new("x.mp4"), 7, None).unwrap();
        assert_eq!((p["format"].as_str(), p["crf"].as_u64(), p["sequence"].as_u64()), (Some("h264"), Some(23), Some(7)));
        assert_eq!(p["bitrateMode"], "crf");
        assert_eq!(p["hardwareEncoding"], "auto");
        assert_eq!(p["wait"], false);
        let p = export_params(&spec(VideoCodec::H264, HwAccel::Cpu, 17), Path::new("x.mp4"), 7, None).unwrap();
        assert_eq!(p["hardwareEncoding"], "off");
        // HEVC has no CRF in FilmCraft: it needs a size to derive a VBR bitrate, and never sends crf.
        assert!(export_params(&spec(VideoCodec::Hevc, HwAccel::Auto, 26), Path::new("x.mp4"), 7, None).is_err());
        let p = export_params(&spec(VideoCodec::Hevc, HwAccel::Auto, 26), Path::new("x.mp4"), 7, Some(&d)).unwrap();
        assert_eq!((p["format"].as_str(), p["bitrateMode"].as_str()), (Some("hevc"), Some("vbr1Pass")));
        assert!(p.get("crf").is_none());
        let kbps = p["bitrateKbps"].as_u64().unwrap();
        assert!((5_000..=7_500).contains(&kbps), "1080p30 at q26 should be ~6 Mbps, got {kbps}");
    }

    #[test]
    fn hevc_bitrate_follows_quality_and_size() {
        let hd = Dims { width: 1920, height: 1080, fps: 30.0 };
        let (hi, mid, lo) = (hevc_bitrate_kbps(&hd, 20), hevc_bitrate_kbps(&hd, 26), hevc_bitrate_kbps(&hd, 31));
        assert!(hi > mid && mid > lo, "{hi} {mid} {lo}");
        let uhd = Dims { width: 3840, height: 2160, fps: 30.0 };
        assert!(hevc_bitrate_kbps(&uhd, 26) > 3 * mid);
        let huge = Dims { width: 7680, height: 4320, fps: 120.0 };
        assert_eq!(hevc_bitrate_kbps(&huge, 10), 120_000); // clamped
        assert_eq!(parse_dims(&json!({"output": {"width": 1920, "height": 1080, "fps": "29.97"}})), Some(Dims { width: 1920, height: 1080, fps: 29.97 }));
    }

    #[test]
    fn unsupported_codecs_are_refused_clearly() {
        assert!(export_params(&spec(VideoCodec::Av1, HwAccel::Auto, 30), Path::new("x"), 1, None).unwrap_err().contains("AV1"));
        assert!(export_params(&spec(VideoCodec::Copy, HwAccel::Auto, 23), Path::new("x"), 1, None).is_err());
    }

    #[test]
    fn finished_job_interpretation() {
        let ok = json!({"finished": true, "offlineMedia": [], "result": {"bytes": 10, "frames": 5}});
        assert!(finish(&ok).is_ok());
        let offline = json!({"finished": true, "offlineMedia": ["a.mov", "b.mov", "c.mov", "d.mov"], "result": {"bytes": 1}});
        let ExecError::Failed(m) = finish(&offline).unwrap_err() else { panic!() };
        assert!(m.contains("4 media") && m.contains("and 1 more"), "{m}");
        let none = json!({"finished": true, "offlineMedia": [], "result": null, "status": "Cancelled"});
        assert!(matches!(finish(&none), Err(ExecError::Failed(m)) if m == "Cancelled"));
    }

    #[tokio::test]
    #[ignore = "needs FilmCraft installed and FILMCRAFT_TEST_PROJECT/FILMCRAFT_TEST_SEQUENCE/FILMCRAFT_TEST_OUT set"]
    async fn renders_a_real_sequence() {
        let project: PathBuf = std::env::var("FILMCRAFT_TEST_PROJECT").unwrap().into();
        let seq: u64 = std::env::var("FILMCRAFT_TEST_SEQUENCE").unwrap().parse().unwrap();
        let out: PathBuf = std::env::var("FILMCRAFT_TEST_OUT").unwrap().into();
        let _ = std::fs::remove_file(&out);
        let mut s = spec(VideoCodec::H264, HwAccel::Auto, 23);
        s.input = project.clone();
        s.output = out.clone();
        s.filmcraft = Some(FilmcraftSource { project, sequence: seq, sequence_name: "t".into() });
        let seen = Arc::new(std::sync::Mutex::new(Vec::<f32>::new()));
        let seen2 = seen.clone();
        let progress: ProgressFn = Arc::new(move |p| {
            if let Some(x) = p.percent {
                seen2.lock().unwrap().push(x);
            }
        });
        let ex = FilmcraftExecutor::new(find_cli());
        ex.execute(s, progress, CancellationToken::new()).await.expect("render");
        assert!(out.is_file());
        let seen = seen.lock().unwrap();
        assert!(seen.len() > 2, "expected several progress updates, got {seen:?}");
        assert!(seen.windows(2).all(|w| w[1] >= w[0] - 0.01), "progress went backwards: {seen:?}");
    }
}
