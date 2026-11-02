//! FFmpeg process wrapper: argument building, progress parsing, hardware probing, execution.

use std::{
    collections::VecDeque,
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
};

use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::Command,
};
use tokio_util::sync::CancellationToken;

use crate::{
    executor::{BoxFuture, ExecError, Executor, ProgressFn},
    job::{AudioCodec, HwAccel, JobSpec, Progress, VideoCodec},
};

/// Spawns helper processes without flashing a console window (matters for the GUI app on Windows).
pub fn no_window(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    cmd
}

#[derive(Clone, Debug)]
pub struct FfmpegConfig {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    /// Passed as `-threads` so N concurrent software encodes don't each grab every core.
    pub threads_per_job: usize,
}

impl Default for FfmpegConfig {
    fn default() -> Self {
        Self { ffmpeg: "ffmpeg".into(), ffprobe: "ffprobe".into(), threads_per_job: 0 }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HwCaps {
    pub nvenc: bool,
    pub cpu_threads: usize,
    pub ffmpeg_version: Option<String>,
}

impl HwCaps {
    /// Probes for a *working* NVENC encoder (a test encode, not just a listed codec).
    pub async fn detect(cfg: &FfmpegConfig) -> Self {
        let cpu_threads = std::thread::available_parallelism().map_or(1, |n| n.get());
        let ffmpeg_version = ffmpeg_version(cfg).await;
        let nvenc = if ffmpeg_version.is_some() {
            no_window(&mut Command::new(&cfg.ffmpeg))
                .args(["-hide_banner", "-loglevel", "error", "-f", "lavfi", "-i"])
                .arg("color=black:s=256x256:d=0.1")
                .args(["-c:v", "h264_nvenc", "-f", "null", "-"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
                .await
                .is_ok_and(|s| s.success())
        } else {
            false
        };
        Self { nvenc, cpu_threads, ffmpeg_version }
    }

    /// Hardware encoders have a small number of concurrent sessions; CPU encoders already
    /// use many threads each, so only a few concurrent jobs make sense.
    pub fn default_concurrency(&self) -> usize {
        if self.nvenc {
            3
        } else {
            (self.cpu_threads / 8).clamp(1, 4)
        }
    }

    pub fn default_threads_per_job(&self, concurrency: usize) -> usize {
        (self.cpu_threads / concurrency.max(1)).max(1)
    }
}

async fn ffmpeg_version(cfg: &FfmpegConfig) -> Option<String> {
    let out = no_window(&mut Command::new(&cfg.ffmpeg)).arg("-version").stdin(Stdio::null()).output().await.ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).lines().next().map(str::to_owned)
}

/// Build the ffmpeg argument list for a job writing to `tmp_output`.
pub fn build_args(
    spec: &JobSpec,
    tmp_output: &Path,
    hw: &HwCaps,
    threads: usize,
) -> Vec<OsString> {
    let mut a: Vec<OsString> = Vec::new();
    let mut push = |s: &str| a.push(s.into());

    push("-hide_banner");
    push("-nostdin");
    push("-nostats");
    push("-loglevel");
    push("error");
    push("-progress");
    push("pipe:1");
    push("-y"); // we always write to our own temp file; overwrite policy is checked earlier

    // The `file:` prefix stops ffmpeg interpreting a path as a protocol (concat:, http:, ...).
    let mut input = OsString::from("file:");
    input.push(&spec.input);
    a.extend(["-i".into(), input]);
    a.extend(["-map".into(), "0:v:0".into(), "-map".into(), "0:a?".into()]);

    let use_nvenc = match spec.accel {
        HwAccel::Cpu => false,
        HwAccel::Nvenc => true,
        HwAccel::Auto => hw.nvenc && matches!(spec.video, VideoCodec::H264 | VideoCodec::Hevc),
    };
    let q = spec.quality.to_string();
    let mut v = |args: &[&str]| a.extend(args.iter().map(OsString::from));
    match (spec.video, use_nvenc) {
        (VideoCodec::Copy, _) => v(&["-c:v", "copy"]),
        (VideoCodec::H264, false) => v(&["-c:v", "libx264", "-preset", "medium", "-crf", &q]),
        (VideoCodec::Hevc, false) => v(&["-c:v", "libx265", "-preset", "medium", "-crf", &q]),
        (VideoCodec::Av1, false) => v(&["-c:v", "libsvtav1", "-crf", &q]),
        (codec, true) => {
            let enc = match codec {
                VideoCodec::H264 => "h264_nvenc",
                VideoCodec::Hevc => "hevc_nvenc",
                _ => "av1_nvenc",
            };
            v(&["-c:v", enc, "-rc", "vbr", "-cq", &q, "-b:v", "0"]);
        }
    }
    if spec.video != VideoCodec::Copy && !use_nvenc && threads > 0 {
        v(&["-threads", &threads.to_string()]);
    }
    match spec.audio {
        AudioCodec::Copy => v(&["-c:a", "copy"]),
        AudioCodec::Aac { bitrate_kbps } => v(&["-c:a", "aac", "-b:a", &format!("{bitrate_kbps}k")]),
    }
    a.push(tmp_output.into());
    a
}

/// `<dir>/.<name>.encraft-part.<ext>`: same directory (so the final rename is atomic, which
/// matters on SMB/NFS) and same extension (so ffmpeg still infers the container).
pub fn temp_output_path(output: &Path) -> PathBuf {
    let name = output.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = output.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    output.with_file_name(format!(".{name}.encraft-part{ext}"))
}

/// Accumulates `key=value` lines from `-progress` output; yields on `progress=` markers.
#[derive(Default)]
pub struct ProgressParser {
    current: Progress,
    total_ms: Option<u64>,
}

impl ProgressParser {
    pub fn new(total_ms: Option<u64>) -> Self {
        Self { current: Progress::default(), total_ms }
    }

    pub fn feed(&mut self, line: &str) -> Option<Progress> {
        let (key, value) = line.split_once('=')?;
        let value = value.trim();
        match key.trim() {
            "frame" => self.current.frame = value.parse().unwrap_or(self.current.frame),
            "fps" => self.current.fps = value.parse().ok(),
            "speed" => self.current.speed = value.trim_end_matches('x').trim().parse().ok(),
            // Despite the name, out_time_ms is in microseconds in ffmpeg's output.
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<i64>() {
                    self.current.out_time_ms = (us.max(0) / 1000) as u64;
                }
            }
            "progress" => {
                self.current.percent = self.total_ms.filter(|t| *t > 0).map(|t| {
                    (self.current.out_time_ms as f32 / t as f32 * 100.0).clamp(0.0, 99.9)
                });
                return Some(self.current.clone());
            }
            _ => {}
        }
        None
    }
}

async fn probe_duration_ms(cfg: &FfmpegConfig, input: &Path) -> Option<u64> {
    let mut path = OsString::from("file:");
    path.push(input);
    let out = no_window(&mut Command::new(&cfg.ffprobe))
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(path)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    let secs: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some((secs * 1000.0) as u64)
}

/// What `ffprobe` reports about the first video stream of a file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaProbe {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_secs: f64,
}

#[derive(Deserialize)]
struct ProbeJson {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    format: Option<ProbeFormat>,
}
#[derive(Deserialize)]
struct ProbeStream {
    width: Option<u32>,
    height: Option<u32>,
    r_frame_rate: Option<String>,
}
#[derive(Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/').unwrap_or((s, "1"));
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (d > 0.0 && n > 0.0).then(|| n / d)
}

fn parse_probe(json: &[u8]) -> Option<MediaProbe> {
    let p: ProbeJson = serde_json::from_slice(json).ok()?;
    let s = p.streams.first()?;
    Some(MediaProbe {
        width: s.width?,
        height: s.height?,
        fps: parse_rate(s.r_frame_rate.as_deref()?)?,
        duration_secs: p.format?.duration?.parse().ok()?,
    })
}

/// Probes a file's first video stream. `None` if it isn't readable media.
pub async fn probe_media(cfg: &FfmpegConfig, path: &Path) -> Option<MediaProbe> {
    let mut input = OsString::from("file:");
    input.push(path);
    let out = no_window(&mut Command::new(&cfg.ffprobe))
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries"])
        .arg("stream=width,height,r_frame_rate:format=duration")
        .args(["-of", "json"])
        .arg(input)
        .stdin(Stdio::null())
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    parse_probe(&out.stdout)
}
pub struct FfmpegExecutor {
    cfg: FfmpegConfig,
    hw: HwCaps,
}

impl FfmpegExecutor {
    pub fn new(cfg: FfmpegConfig, hw: HwCaps) -> Self {
        Self { cfg, hw }
    }

    async fn run(
        &self,
        spec: JobSpec,
        on_progress: ProgressFn,
        cancel: CancellationToken,
    ) -> Result<(), ExecError> {
        let fail = |m: String| ExecError::Failed(m);

        if !spec.input.is_file() {
            return Err(fail(format!("input not found: {}", spec.input.display())));
        }
        if !spec.overwrite && spec.output.exists() {
            return Err(fail(format!("output already exists: {}", spec.output.display())));
        }

        if let Some(dir) = spec.output.parent() {
            tokio::fs::create_dir_all(dir).await.map_err(|e| fail(format!("cannot create {}: {e}", dir.display())))?;
        }
        let tmp = temp_output_path(&spec.output);
        let total_ms = probe_duration_ms(&self.cfg, &spec.input).await;
        let args = build_args(&spec, &tmp, &self.hw, self.cfg.threads_per_job);

        let mut child = no_window(&mut Command::new(&self.cfg.ffmpeg))
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| fail(format!("failed to start ffmpeg: {e}")))?;

        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");

        let stderr_task = tokio::spawn(async move {
            let mut tail: VecDeque<String> = VecDeque::with_capacity(20);
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if tail.len() == 20 {
                    tail.pop_front();
                }
                tail.push_back(line);
            }
            tail.into_iter().collect::<Vec<_>>().join("\n")
        });

        let progress_task = tokio::spawn(async move {
            let mut parser = ProgressParser::new(total_ms);
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(p) = parser.feed(&line) {
                    on_progress(p);
                }
            }
        });

        let status = tokio::select! {
            s = child.wait() => s.map_err(|e| fail(format!("waiting for ffmpeg: {e}")))?,
            _ = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(ExecError::Cancelled);
            }
        };
        let _ = progress_task.await;
        let stderr_tail = stderr_task.await.unwrap_or_default();

        if !status.success() {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(fail(format!("ffmpeg exited with {status}: {stderr_tail}")));
        }
        tokio::fs::rename(&tmp, &spec.output).await.map_err(|e| {
            fail(format!("finalizing {}: {e}", spec.output.display()))
        })
    }
}

impl Executor for FfmpegExecutor {
    fn execute(
        &self,
        spec: JobSpec,
        on_progress: ProgressFn,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<(), ExecError>> {
        Box::pin(self.run(spec, on_progress, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(video: VideoCodec, accel: HwAccel) -> JobSpec {
        JobSpec {
            input: "/in/a.mkv".into(),
            output: "/out/a.mp4".into(),
            video,
            accel,
            quality: 20,
            audio: AudioCodec::Aac { bitrate_kbps: 128 },
            overwrite: false,
            filmcraft: None,
        }
    }

    fn args(spec: &JobSpec, nvenc: bool) -> Vec<String> {
        let hw = HwCaps { nvenc, cpu_threads: 8, ffmpeg_version: None };
        build_args(spec, Path::new("/out/.a.encraft-part.mp4"), &hw, 4)
            .into_iter()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parses_ffprobe_json() {
        let j = br#"{"streams":[{"width":1920,"height":1080,"r_frame_rate":"24000/1001"}],"format":{"duration":"76.672000"}}"#;
        let p = parse_probe(j).unwrap();
        assert_eq!((p.width, p.height), (1920, 1080));
        assert!((p.fps - 23.976).abs() < 0.001);
        assert_eq!(p.duration_secs, 76.672);
        assert!(parse_probe(br#"{"streams":[],"format":{}}"#).is_none());
        assert!(parse_probe(b"garbage").is_none());
    }

    #[test]
    fn input_is_protocol_pinned() {
        let a = args(&spec(VideoCodec::H264, HwAccel::Cpu), false);
        assert!(a.contains(&"file:/in/a.mkv".to_string()));
    }

    #[test]
    fn auto_picks_nvenc_only_when_available() {
        assert!(args(&spec(VideoCodec::Hevc, HwAccel::Auto), true).contains(&"hevc_nvenc".into()));
        assert!(args(&spec(VideoCodec::Hevc, HwAccel::Auto), false).contains(&"libx265".into()));
        // AV1 never auto-selects NVENC (needs Ada or newer).
        assert!(args(&spec(VideoCodec::Av1, HwAccel::Auto), true).contains(&"libsvtav1".into()));
    }

    #[test]
    fn explicit_cpu_ignores_nvenc() {
        assert!(args(&spec(VideoCodec::H264, HwAccel::Cpu), true).contains(&"libx264".into()));
    }

    #[test]
    fn temp_path_keeps_dir_and_extension() {
        assert_eq!(
            temp_output_path(Path::new("/mnt/nas/out/movie.mkv")),
            PathBuf::from("/mnt/nas/out/.movie.encraft-part.mkv")
        );
    }

    #[test]
    fn parses_progress_blocks() {
        let mut p = ProgressParser::new(Some(10_000));
        let block = "frame=120\nfps=30.0\nout_time_us=5000000\nspeed=2.5x\nprogress=continue";
        let mut last = None;
        for line in block.lines() {
            last = p.feed(line).or(last);
        }
        let got = last.unwrap();
        assert_eq!(got.frame, 120);
        assert_eq!(got.out_time_ms, 5000);
        assert_eq!(got.speed, Some(2.5));
        assert_eq!(got.percent, Some(50.0));
    }

    #[test]
    fn unknown_duration_gives_no_percent() {
        let mut p = ProgressParser::new(None);
        p.feed("out_time_us=1000");
        assert_eq!(p.feed("progress=continue").unwrap().percent, None);
    }
}
