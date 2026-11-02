//! Job queue: bounded worker pool, event stream, cancellation and crash-safe snapshots.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::{
    executor::{ExecError, Executor, ProgressFn},
    job::{InvalidTransition, Job, JobId, JobSpec, JobState, Progress},
};

#[derive(Clone, Debug)]
pub struct QueueConfig {
    /// Maximum number of jobs encoding at once.
    pub max_concurrent: usize,
    /// If set, job state is persisted here and restored on start.
    pub snapshot_path: Option<PathBuf>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self { max_concurrent: 1, snapshot_path: None }
    }
}

#[derive(Clone, Debug)]
pub enum JobEvent {
    /// The job was added, changed state, or made progress.
    Updated(Box<Job>),
    /// The job was removed from the queue's history.
    Removed(JobId),
}

#[derive(Debug, thiserror::Error)]
pub enum QueueError {
    #[error("job not found")]
    NotFound,
    #[error("job is {0:?}; only queued or processing jobs can be cancelled")]
    NotCancellable(JobState),
    #[error("job is still active; cancel it first")]
    StillActive,
    #[error(transparent)]
    Transition(#[from] InvalidTransition),
}

struct Entry {
    job: Job,
    cancel: CancellationToken,
}

struct Inner {
    jobs: Mutex<HashMap<JobId, Entry>>,
    pending_tx: mpsc::UnboundedSender<JobId>,
    events: broadcast::Sender<JobEvent>,
    executor: Arc<dyn Executor>,
    snapshot_path: Option<PathBuf>,
    snapshot_lock: tokio::sync::Mutex<()>,
    shutdown: CancellationToken,
    max_concurrent: usize,
}

impl Inner {
    fn jobs(&self) -> MutexGuard<'_, HashMap<JobId, Entry>> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn emit(&self, job: &Job) {
        // No receivers is fine.
        let _ = self.events.send(JobEvent::Updated(Box::new(job.clone())));
    }

    async fn persist(&self) {
        let Some(path) = &self.snapshot_path else { return };
        let _guard = self.snapshot_lock.lock().await;
        let mut jobs: Vec<Job> = self.jobs().values().map(|e| e.job.clone()).collect();
        jobs.sort_by_key(|j| j.created_at);
        let result = async {
            let bytes = serde_json::to_vec_pretty(&jobs)?;
            let tmp = path.with_extension("tmp");
            tokio::fs::write(&tmp, bytes).await?;
            tokio::fs::rename(&tmp, path).await?;
            Ok::<_, Box<dyn std::error::Error>>(())
        }
        .await;
        if let Err(e) = result {
            tracing::warn!("failed to write queue snapshot {}: {e}", path.display());
        }
    }
}

/// Cheap to clone; all clones share the same queue.
#[derive(Clone)]
pub struct JobQueue {
    inner: Arc<Inner>,
}

impl JobQueue {
    /// Creates the queue and spawns its dispatcher on the current Tokio runtime.
    pub async fn start(config: QueueConfig, executor: Arc<dyn Executor>) -> Self {
        let (pending_tx, pending_rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1024);
        let max_concurrent = config.max_concurrent.max(1);

        let mut restored = load_snapshot(config.snapshot_path.as_deref()).await;
        restored.sort_by_key(|j| j.created_at);
        let mut jobs = HashMap::new();
        for mut job in restored {
            if job.state == JobState::Processing {
                tracing::info!(%job.id, "re-queueing job interrupted by restart");
                let _ = job.transition(JobState::Queued);
            }
            if job.state == JobState::Queued {
                let _ = pending_tx.send(job.id);
            }
            jobs.insert(job.id, Entry { job, cancel: CancellationToken::new() });
        }

        let inner = Arc::new(Inner {
            jobs: Mutex::new(jobs),
            pending_tx,
            events,
            executor,
            snapshot_path: config.snapshot_path,
            snapshot_lock: tokio::sync::Mutex::new(()),
            shutdown: CancellationToken::new(),
            max_concurrent,
        });
        tokio::spawn(dispatch(inner.clone(), pending_rx));
        Self { inner }
    }

    pub fn max_concurrent(&self) -> usize {
        self.inner.max_concurrent
    }

    pub async fn submit(&self, spec: JobSpec) -> Job {
        let job = Job::new(spec);
        self.inner
            .jobs()
            .insert(job.id, Entry { job: job.clone(), cancel: CancellationToken::new() });
        self.inner.emit(&job);
        self.inner.persist().await;
        // The dispatcher only goes away on shutdown, in which case nothing will run anyway.
        let _ = self.inner.pending_tx.send(job.id);
        job
    }

    pub fn get(&self, id: JobId) -> Option<Job> {
        self.inner.jobs().get(&id).map(|e| e.job.clone())
    }

    /// All jobs, oldest first.
    pub fn list(&self) -> Vec<Job> {
        let mut jobs: Vec<Job> = self.inner.jobs().values().map(|e| e.job.clone()).collect();
        jobs.sort_by_key(|j| j.created_at);
        jobs
    }

    pub fn subscribe(&self) -> broadcast::Receiver<JobEvent> {
        self.inner.events.subscribe()
    }

    /// Cancels a queued job immediately, or signals a running job to stop.
    pub async fn cancel(&self, id: JobId) -> Result<(), QueueError> {
        let cancelled_now = {
            let mut jobs = self.inner.jobs();
            let entry = jobs.get_mut(&id).ok_or(QueueError::NotFound)?;
            match entry.job.state {
                JobState::Queued => {
                    entry.job.transition(JobState::Cancelled)?;
                    self.inner.emit(&entry.job);
                    true
                }
                JobState::Processing => {
                    entry.cancel.cancel();
                    false
                }
                other => return Err(QueueError::NotCancellable(other)),
            }
        };
        if cancelled_now {
            self.inner.persist().await;
        }
        Ok(())
    }

    /// Removes a finished job from the history.
    pub async fn remove(&self, id: JobId) -> Result<(), QueueError> {
        {
            let mut jobs = self.inner.jobs();
            let entry = jobs.get(&id).ok_or(QueueError::NotFound)?;
            if !entry.job.state.is_terminal() {
                return Err(QueueError::StillActive);
            }
            jobs.remove(&id);
        }
        let _ = self.inner.events.send(JobEvent::Removed(id));
        self.inner.persist().await;
        Ok(())
    }

    /// Stops dispatching and cancels running jobs. Running jobs are left `Processing` in the
    /// snapshot so they are re-queued on the next start.
    pub fn shutdown(&self) {
        self.inner.shutdown.cancel();
    }
}

async fn load_snapshot(path: Option<&std::path::Path>) -> Vec<Job> {
    let Some(path) = path else { return Vec::new() };
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::error!("ignoring corrupt queue snapshot {}: {e}", path.display());
            Vec::new()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => {
            tracing::error!("cannot read queue snapshot {}: {e}", path.display());
            Vec::new()
        }
    }
}

async fn dispatch(inner: Arc<Inner>, mut pending: mpsc::UnboundedReceiver<JobId>) {
    let permits = Arc::new(Semaphore::new(inner.max_concurrent));
    loop {
        // Take a slot first so we never start more than `max_concurrent` jobs.
        let permit = tokio::select! {
            p = permits.clone().acquire_owned() => p.expect("semaphore never closed"),
            _ = inner.shutdown.cancelled() => return,
        };
        let id = tokio::select! {
            id = pending.recv() => match id { Some(id) => id, None => return },
            _ = inner.shutdown.cancelled() => return,
        };
        let inner = inner.clone();
        tokio::spawn(async move {
            run_job(&inner, id).await;
            drop(permit);
        });
    }
}

async fn run_job(inner: &Arc<Inner>, id: JobId) {
    // Claim the job. It may have been cancelled or removed while waiting in the queue.
    let (spec, cancel) = {
        let mut jobs = inner.jobs();
        let Some(entry) = jobs.get_mut(&id) else { return };
        if entry.job.state != JobState::Queued {
            return;
        }
        entry.job.transition(JobState::Processing).expect("queued -> processing is valid");
        entry.cancel = inner.shutdown.child_token();
        inner.emit(&entry.job);
        (entry.job.spec.clone(), entry.cancel.clone())
    };
    inner.persist().await;

    let on_progress: ProgressFn = {
        let inner = inner.clone();
        Arc::new(move |p: Progress| {
            let mut jobs = inner.jobs();
            if let Some(entry) = jobs.get_mut(&id) {
                if entry.job.state == JobState::Processing {
                    entry.job.progress = p;
                    inner.emit(&entry.job);
                }
            }
        })
    };

    let result = inner.executor.execute(spec, on_progress, cancel).await;

    if inner.shutdown.is_cancelled() && matches!(result, Err(ExecError::Cancelled)) {
        // Daemon is stopping; leave the job Processing so the snapshot re-queues it.
        return;
    }
    {
        let mut jobs = inner.jobs();
        let Some(entry) = jobs.get_mut(&id) else { return };
        let outcome = match result {
            Ok(()) => entry.job.transition(JobState::Completed),
            Err(ExecError::Cancelled) => entry.job.transition(JobState::Cancelled),
            Err(ExecError::Failed(msg)) => entry.job.fail(msg),
        };
        if let Err(e) = outcome {
            tracing::error!(%id, "could not finalize job: {e}");
            return;
        }
        inner.emit(&entry.job);
    }
    inner.persist().await;
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicUsize, Ordering},
        time::Duration,
    };

    use tokio::sync::Notify;

    use super::*;
    use crate::{
        executor::BoxFuture,
        job::{AudioCodec, HwAccel, VideoCodec},
    };

    fn spec(n: usize) -> JobSpec {
        JobSpec {
            input: format!("/in/{n}.mkv").into(),
            output: format!("/out/{n}.mp4").into(),
            video: VideoCodec::H264,
            accel: HwAccel::Cpu,
            quality: 23,
            audio: AudioCodec::Copy,
            overwrite: false,
            filmcraft: None,
        }
    }

    #[derive(Default)]
    struct Mock {
        running: AtomicUsize,
        peak: AtomicUsize,
        gate: Notify,
        fail_input: Option<String>,
    }

    impl Executor for Mock {
        fn execute(
            &self,
            spec: JobSpec,
            on_progress: ProgressFn,
            cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<(), ExecError>> {
            Box::pin(async move {
                let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(now, Ordering::SeqCst);
                on_progress(Progress { percent: Some(50.0), ..Default::default() });
                let r = tokio::select! {
                    _ = self.gate.notified() => Ok(()),
                    _ = cancel.cancelled() => Err(ExecError::Cancelled),
                };
                self.running.fetch_sub(1, Ordering::SeqCst);
                if r.is_ok() && self.fail_input.as_deref() == spec.input.to_str() {
                    return Err(ExecError::Failed("boom".into()));
                }
                r
            })
        }
    }

    async fn wait_for(queue: &JobQueue, id: JobId, state: JobState) -> Job {
        for _ in 0..200 {
            if let Some(j) = queue.get(id).filter(|j| j.state == state) {
                return j;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("job {id} never reached {state:?}: {:?}", queue.get(id));
    }

    #[tokio::test]
    async fn concurrency_is_bounded() {
        let mock = Arc::new(Mock::default());
        let q = JobQueue::start(QueueConfig { max_concurrent: 2, ..Default::default() }, mock.clone()).await;
        let ids: Vec<_> = futures_ids(&q, 5).await;

        wait_for(&q, ids[0], JobState::Processing).await;
        wait_for(&q, ids[1], JobState::Processing).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(q.get(ids[2]).unwrap().state, JobState::Queued);

        for _ in 0..5 {
            mock.gate.notify_one();
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        for id in ids {
            wait_for(&q, id, JobState::Completed).await;
        }
        assert_eq!(mock.peak.load(Ordering::SeqCst), 2);
    }

    async fn futures_ids(q: &JobQueue, n: usize) -> Vec<JobId> {
        let mut ids = Vec::new();
        for i in 0..n {
            ids.push(q.submit(spec(i)).await.id);
        }
        ids
    }

    #[tokio::test]
    async fn cancel_queued_and_running() {
        let mock = Arc::new(Mock::default());
        let q = JobQueue::start(QueueConfig::default(), mock).await;
        let running = q.submit(spec(0)).await.id;
        let queued = q.submit(spec(1)).await.id;
        wait_for(&q, running, JobState::Processing).await;

        q.cancel(queued).await.unwrap();
        assert_eq!(q.get(queued).unwrap().state, JobState::Cancelled);
        q.cancel(running).await.unwrap();
        wait_for(&q, running, JobState::Cancelled).await;

        assert!(matches!(q.cancel(running).await, Err(QueueError::NotCancellable(_))));
    }

    #[tokio::test]
    async fn failure_is_recorded() {
        let mock = Arc::new(Mock { fail_input: Some("/in/0.mkv".into()), ..Default::default() });
        let q = JobQueue::start(QueueConfig::default(), mock.clone()).await;
        let id = q.submit(spec(0)).await.id;
        wait_for(&q, id, JobState::Processing).await;
        mock.gate.notify_one();
        let job = wait_for(&q, id, JobState::Failed).await;
        assert_eq!(job.error.as_deref(), Some("boom"));
    }

    #[tokio::test]
    async fn interrupted_jobs_are_requeued_after_restart() {
        let dir = std::env::temp_dir().join(format!("encraft-test-{}", JobId::new()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let cfg = QueueConfig { max_concurrent: 1, snapshot_path: Some(dir.join("q.json")) };

        let mock = Arc::new(Mock::default());
        let q = JobQueue::start(cfg.clone(), mock).await;
        let id = q.submit(spec(0)).await.id;
        wait_for(&q, id, JobState::Processing).await;
        q.shutdown(); // simulates a daemon stop mid-encode
        tokio::time::sleep(Duration::from_millis(50)).await;

        let mock2 = Arc::new(Mock::default());
        let q2 = JobQueue::start(cfg, mock2.clone()).await;
        wait_for(&q2, id, JobState::Processing).await;
        mock2.gate.notify_one();
        wait_for(&q2, id, JobState::Completed).await;
        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn cannot_remove_active_job() {
        let q = JobQueue::start(QueueConfig::default(), Arc::new(Mock::default())).await;
        let id = q.submit(spec(0)).await.id;
        assert!(matches!(q.remove(id).await, Err(QueueError::StillActive)));
        q.cancel(id).await.unwrap();
        wait_for(&q, id, JobState::Cancelled).await;
        q.remove(id).await.unwrap();
        assert!(q.get(id).is_none());
    }
}
