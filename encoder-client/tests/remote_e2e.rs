//! End-to-end test of the client against a *running* `encraft-d`.
//!
//! Ignored by default. Start the daemon, then:
//!   ENCRAFT_E2E_ADDR=127.0.0.1:7878 ENCRAFT_E2E_TOKEN=<token> ENCRAFT_E2E_ROOT=<media root> \
//!     cargo test -p encoder-client --test remote_e2e -- --ignored
//!
//! File names steer the (fake or real) encoder: a name containing "slow" runs long, "fail" fails.

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use encoder_client::remote::{RemoteClient, RemoteEvent};
use encoder_core::{
    api::WsMessage,
    job::{AudioCodec, HwAccel, VideoCodec},
    Job, JobId, JobSpec, JobState,
};
use tokio::sync::mpsc::UnboundedReceiver;

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("set {name} (see file header)"))
}

/// Real encoders need real media: keep a pre-made input if the caller supplied one.
fn write_if_missing(p: &Path) {
    if !p.exists() {
        std::fs::write(p, b"input").unwrap();
    }
}

fn spec(input: &Path, output: &Path) -> JobSpec {
    JobSpec {
        input: input.into(),
        output: output.into(),
        video: VideoCodec::H264,
        accel: HwAccel::Auto,
        quality: 23,
        audio: AudioCodec::Copy,
        overwrite: true,
        filmcraft: None,
    }
}

/// Folds the event stream into current job state, like the UI does.
struct Tracker {
    rx: UnboundedReceiver<RemoteEvent>,
    jobs: HashMap<JobId, Job>,
    saw_percent: HashMap<JobId, bool>,
    removed: Vec<JobId>,
    snapshots: u32,
}

impl Tracker {
    async fn until(&mut self, what: &str, mut done: impl FnMut(&Tracker) -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        while !done(self) {
            let ev = tokio::time::timeout_at(deadline, self.rx.recv())
                .await
                .unwrap_or_else(|_| panic!("timed out waiting for: {what}"))
                .expect("event stream closed");
            let RemoteEvent::Message(m) = ev else { continue };
            match m {
                WsMessage::Snapshot { jobs } => {
                    self.snapshots += 1;
                    self.jobs = jobs.into_iter().map(|j| (j.id, j)).collect();
                }
                WsMessage::JobUpdated { job } => {
                    if job.state == JobState::Processing && job.progress.percent.is_some_and(|p| p > 0.0) {
                        self.saw_percent.insert(job.id, true);
                    }
                    self.jobs.insert(job.id, job);
                }
                WsMessage::JobRemoved { id } => {
                    self.jobs.remove(&id);
                    self.removed.push(id);
                }
                WsMessage::Lagged => {}
            }
        }
    }

    fn state(&self, id: JobId) -> Option<JobState> {
        self.jobs.get(&id).map(|j| j.state)
    }
}

#[tokio::test]
#[ignore = "needs a running encraft-d; see file header"]
async fn full_lifecycle_against_running_daemon() {
    let addr: SocketAddr = env("ENCRAFT_E2E_ADDR").parse().unwrap();
    let root = PathBuf::from(env("ENCRAFT_E2E_ROOT"));
    let client = RemoteClient::new(addr, env("ENCRAFT_E2E_TOKEN"));

    // Node info
    let info = client.node_info().await.expect("node_info");
    assert!(info.hardware.ffmpeg_version.is_some(), "daemon found no ffmpeg");
    assert!(info.max_concurrent >= 1);

    // Auth is enforced
    let bad = RemoteClient::new(addr, "not-the-token".into());
    assert!(bad.list_jobs().await.is_err(), "wrong token must be rejected");

    // Path restriction is enforced
    let outside = std::env::temp_dir().join("encraft-e2e-outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("x.mov"), b"x").unwrap();
    let err = client.submit(vec![spec(&outside.join("x.mov"), &root.join("o.mp4"))]).await.unwrap_err();
    assert!(err.to_string().contains("outside the allowed media roots"), "{err}");

    // Live event stream
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stream = client.spawn_event_stream(tx);
    let mut t = Tracker { rx, jobs: HashMap::new(), saw_percent: HashMap::new(), removed: vec![], snapshots: 0 };
    t.until("initial snapshot", |t| t.snapshots >= 1).await;

    // 1. Happy path
    let (i1, o1) = (root.join("clip.mov"), root.join("clip.mp4"));
    write_if_missing(&i1);
    let _ = std::fs::remove_file(&o1);
    let id1 = client.submit(vec![spec(&i1, &o1)]).await.expect("submit")[0].id;
    t.until("job 1 completed", |t| t.state(id1) == Some(JobState::Completed)).await;
    assert!(t.saw_percent.get(&id1).copied().unwrap_or(false), "no live progress seen");
    assert_eq!(t.jobs[&id1].progress.percent, Some(100.0));
    assert!(o1.is_file(), "output was not produced");

    // 2. Failure is reported with the encoder's message
    let (i2, o2) = (root.join("fail.mov"), root.join("fail.mp4"));
    std::fs::write(&i2, b"input").unwrap();
    let id2 = client.submit(vec![spec(&i2, &o2)]).await.unwrap()[0].id;
    t.until("job 2 failed", |t| t.state(id2) == Some(JobState::Failed)).await;
    let msg = t.jobs[&id2].error.as_deref().unwrap_or("");
    assert!(msg.contains("ffmpeg exited"), "unexpected error text: {msg:?}");
    assert!(!o2.exists());

    // 3. Cancel a running job; no output or temp file may remain
    let (i3, o3) = (root.join("slow.mov"), root.join("slow.mp4"));
    write_if_missing(&i3);
    let id3 = client.submit(vec![spec(&i3, &o3)]).await.unwrap()[0].id;
    t.until("job 3 processing", |t| t.state(id3) == Some(JobState::Processing)).await;
    client.cancel(id3).await.expect("cancel");
    t.until("job 3 cancelled", |t| t.state(id3) == Some(JobState::Cancelled)).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!o3.exists());
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("encraft-part"))
        .collect();
    assert!(leftovers.is_empty(), "temp files left behind: {leftovers:?}");
    assert!(client.cancel(id3).await.is_err(), "cancelling a finished job should be rejected");

    // 4. Removing a finished job
    client.remove(id1).await.expect("remove");
    t.until("job 1 removed", |t| t.removed.contains(&id1)).await;
    let listed = client.list_jobs().await.unwrap();
    assert!(listed.iter().all(|j| j.id != id1));
    assert!(listed.iter().any(|j| j.id == id2));

    stream.abort();
}
