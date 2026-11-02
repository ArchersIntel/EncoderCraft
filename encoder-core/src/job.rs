//! Job model and state machine.

use std::{fmt, path::PathBuf, str::FromStr};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(Uuid);

impl JobId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for JobId {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl FromStr for JobId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(s).map(Self)
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoCodec {
    H264,
    Hevc,
    Av1,
    /// Stream copy, no re-encode.
    Copy,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HwAccel {
    /// NVENC when the node has a working GPU encoder, otherwise CPU.
    #[default]
    Auto,
    Cpu,
    Nvenc,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "codec")]
pub enum AudioCodec {
    Copy,
    Aac { bitrate_kbps: u32 },
}

impl Default for AudioCodec {
    fn default() -> Self {
        Self::Aac { bitrate_kbps: 160 }
    }
}

/// A sequence inside a FilmCraft project, rendered by FilmCraft itself instead of read from a file.
/// Only the machine that has FilmCraft installed can run such a job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FilmcraftSource {
    pub project: PathBuf,
    pub sequence: u64,
    /// Shown in the UI; not used for rendering.
    #[serde(default)]
    pub sequence_name: String,
}

/// What to encode. Paths are interpreted on the node that runs the job.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JobSpec {
    pub input: PathBuf,
    pub output: PathBuf,
    pub video: VideoCodec,
    #[serde(default)]
    pub accel: HwAccel,
    /// CRF for software encoders, CQ for NVENC. Lower is better quality.
    #[serde(default = "default_quality")]
    pub quality: u8,
    #[serde(default)]
    pub audio: AudioCodec,
    #[serde(default)]
    pub overwrite: bool,
    /// When set, `input` is the project file and the sequence is rendered by FilmCraft.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filmcraft: Option<FilmcraftSource>,
}

fn default_quality() -> u8 {
    23
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Processing,
    Completed,
    Failed,
    Cancelled,
}

impl JobState {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    pub fn can_transition_to(self, next: JobState) -> bool {
        use JobState::*;
        matches!(
            (self, next),
            (Queued, Processing)
                | (Queued, Cancelled)
                | (Processing, Completed)
                | (Processing, Failed)
                | (Processing, Cancelled)
                // Crash recovery: a job that was running when the daemon died is retried.
                | (Processing, Queued)
        )
    }
}

#[derive(Debug, thiserror::Error)]
#[error("invalid job state transition {from:?} -> {to:?}")]
pub struct InvalidTransition {
    pub from: JobState,
    pub to: JobState,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Progress {
    /// 0..=100, absent when the input duration is unknown.
    pub percent: Option<f32>,
    pub frame: u64,
    pub fps: Option<f32>,
    /// Encode speed relative to realtime (e.g. 2.5 = 2.5x).
    pub speed: Option<f32>,
    pub out_time_ms: u64,
    /// Estimated seconds remaining, when the renderer reports one.
    #[serde(default)]
    pub eta_secs: Option<f32>,
    /// Running in the queue but waiting for a shared renderer (e.g. FilmCraft renders one at a time).
    #[serde(default)]
    pub waiting: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Job {
    pub id: JobId,
    pub spec: JobSpec,
    pub state: JobState,
    pub progress: Progress,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

impl Job {
    pub fn new(spec: JobSpec) -> Self {
        Self {
            id: JobId::new(),
            spec,
            state: JobState::Queued,
            progress: Progress::default(),
            error: None,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        }
    }

    pub fn transition(&mut self, next: JobState) -> Result<(), InvalidTransition> {
        if !self.state.can_transition_to(next) {
            return Err(InvalidTransition { from: self.state, to: next });
        }
        match next {
            JobState::Processing => {
                self.started_at = Some(Utc::now());
                self.progress = Progress::default();
            }
            JobState::Queued => {
                self.started_at = None;
                self.progress = Progress::default();
            }
            JobState::Completed => {
                self.progress.percent = Some(100.0);
                self.finished_at = Some(Utc::now());
            }
            JobState::Failed | JobState::Cancelled => self.finished_at = Some(Utc::now()),
        }
        self.state = next;
        Ok(())
    }

    pub fn fail(&mut self, error: impl Into<String>) -> Result<(), InvalidTransition> {
        self.transition(JobState::Failed)?;
        self.error = Some(error.into());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec() -> JobSpec {
        JobSpec {
            input: "/in.mkv".into(),
            output: "/out.mp4".into(),
            video: VideoCodec::H264,
            accel: HwAccel::Auto,
            quality: 23,
            audio: AudioCodec::default(),
            overwrite: false,
            filmcraft: None,
        }
    }

    #[test]
    fn happy_path() {
        let mut job = Job::new(spec());
        job.transition(JobState::Processing).unwrap();
        assert!(job.started_at.is_some());
        job.transition(JobState::Completed).unwrap();
        assert_eq!(job.progress.percent, Some(100.0));
        assert!(job.finished_at.is_some());
    }

    #[test]
    fn terminal_states_are_final() {
        for terminal in [JobState::Completed, JobState::Failed, JobState::Cancelled] {
            for next in [
                JobState::Queued,
                JobState::Processing,
                JobState::Completed,
                JobState::Failed,
                JobState::Cancelled,
            ] {
                assert!(!terminal.can_transition_to(next));
            }
        }
    }

    #[test]
    fn cannot_skip_processing() {
        let mut job = Job::new(spec());
        assert!(job.transition(JobState::Completed).is_err());
        assert!(job.transition(JobState::Failed).is_err());
    }

    #[test]
    fn spec_defaults_deserialize() {
        let s: JobSpec = serde_json::from_str(
            r#"{"input":"/a.mkv","output":"/b.mp4","video":"hevc"}"#,
        )
        .unwrap();
        assert_eq!(s.quality, 23);
        assert_eq!(s.accel, HwAccel::Auto);
        assert!(!s.overwrite);
    }
}
