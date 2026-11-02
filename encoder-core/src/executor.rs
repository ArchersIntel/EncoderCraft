//! Abstraction over "something that runs a job", so the queue can be tested without FFmpeg.

use std::{future::Future, pin::Pin, sync::Arc};

pub use tokio_util::sync::CancellationToken;

use crate::job::{JobSpec, Progress};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("cancelled")]
    Cancelled,
    #[error("{0}")]
    Failed(String),
}

pub type ProgressFn = Arc<dyn Fn(Progress) + Send + Sync>;

pub trait Executor: Send + Sync + 'static {
    fn execute(
        &self,
        spec: JobSpec,
        on_progress: ProgressFn,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<(), ExecError>>;
}
