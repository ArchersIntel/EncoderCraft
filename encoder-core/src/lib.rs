//! Shared types and logic for EncoderCraft: job model, queue, and the FFmpeg wrapper.

pub mod api;
pub mod executor;
pub mod ffmpeg;
pub mod fsutil;
pub mod job;
pub mod queue;

pub use job::{Job, JobId, JobSpec, JobState};
pub use queue::{JobEvent, JobQueue, QueueConfig};
