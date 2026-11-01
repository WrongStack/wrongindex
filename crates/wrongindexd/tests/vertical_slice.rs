//! M1 exit gate: a real named-pipe client connects, authenticates, admits a
//! fixture job, and obtains a durable result — against the real daemon
//! process, real registry SQLite, and a real WrongStack-produced index.db.
//! Nothing here mocks the transport, registry, or inspector.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use wrongindex_contracts::{
    AuthRequest, AuthResponse, ErrorCode, JobAdmission, JobRecord, JobState, MAX_FRAME_BYTES,
    RegisterProjectResult, Response, method,
};
use wrongindex_ipc::frame::{read_frame, write_frame};
use wrongindex_ipc::{CallError, Client, DataRoot, Discovery};

struct Daemon {
    child: Option<Child>,
}

impl Daemon {
    fn spawn(root: &Path) -> Child {
        Command::new(env!("CARGO_BIN_EXE_wrongindexd"))
            .arg("--data-root")
            .arg(root)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn wrongindexd")
    }

    async fn start(root: &Path) -> Self {
        let child = Self::spawn(root);
        wait_ready(root).await;
        Self { child: Some(child) }
    }

    /// Graceful stop through the protocol; asserts the process exits cleanly.
    async fn stop(mut self, root: &Path) {
        let mut c = client(root).await;
        c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
            .await
            .unwrap();
        let mut child = self.child.take().unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "daemon exit status {status}");
                break;
            }
            assert!(Instant::now() < deadline, "daemon did not stop");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            !data_root(root).endpoint_file().exists(),
            "discovery not retracted"
        );
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn data_root(root: &Path) -> DataRoot {
    DataRoot::resolve(Some(root)).unwrap()
}

async fn wait_ready(root: &Path) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if Client::connect(&data_root(root), "test").await.is_ok() {
            return;
        }
        assert!(Instant::now() < deadline, "daemon did not become ready");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn client(root: &Path) -> Client {
    Client::connect(&data_root(root), "test")
        .await
        .expect("connect")
}

fn fixture_index(name: &str, into: &Path) -> PathBuf {
    let src = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/oracle")
        .join(name)
        .join("index.db");
    std::fs::create_dir_all(into).unwrap();
    std::fs::copy(src, into.join("index.db")).unwrap();
    into.join("index.db")
}

fn sha(path: &Path) -> String {
    hex::encode(Sha256::digest(std::fs::read(path).unwrap()))
}

async fn wait_job(c: &mut Client, job_id: &str) -> JobRecord {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let job: JobRecord = c
            .call(method::JOB_GET, None, &json!({"jobId": job_id}))
            .await
            .unwrap();
        if job.state.is_terminal() {
            return job;
        }
        assert!(Instant::now() < deadline, "job did not finish: {job:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn vertical_slice_admits_and_completes_a_durable_check_job() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let db = fixture_index("basic-ts", &tmp.path().join("index"));
    let before = sha(&db);

    let daemon = Daemon::start(&root).await;
    let mut c = client(&root).await;
    assert_eq!(
        c.hello().selected_profile_id.as_deref(),
        Some("wrongstack-index-v5-baseline")
    );

    let reg: RegisterProjectResult = c
        .call(
            method::PROJECT_REGISTER,
            None,
            &json!({"checkoutRoot": checkout, "indexDir": db.parent().unwrap()}),
        )
        .await
        .unwrap();
    assert!(reg.created);
    let pid = reg.project.project_id.clone();

    let key = "retry-1";
    let first: JobAdmission = serde_json::from_value(
        c.call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "check"}),
            Some(key),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(!first.deduplicated);
    let retry: JobAdmission = serde_json::from_value(
        c.call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "check"}),
            Some(key),
        )
        .await
        .unwrap(),
    )
    .unwrap();
    assert!(retry.deduplicated, "idempotent retry must not admit twice");
    assert_eq!(retry.job.job_id, first.job.job_id);

    let done = wait_job(&mut c, &first.job.job_id).await;
    assert_eq!(done.state, JobState::Succeeded, "{done:?}");
    let result = done.result.clone().unwrap();
    assert_eq!(result["verdict"], "compatible");
    assert_eq!(result["writeAdmissible"], true);
    assert_eq!(result["counts"]["symbols"], 14);
    assert_eq!(sha(&db), before, "check job mutated index.db");

    // A finished job is not cancelable; an unknown one is a typed error.
    let cancel = c
        .call_raw(
            method::JOB_CANCEL,
            None,
            json!({"jobId": first.job.job_id}),
            None,
        )
        .await
        .unwrap();
    assert_eq!(cancel["canceled"], false);
    assert_eq!(cancel["job"]["state"], "succeeded");
    let err = c
        .call_raw(
            method::JOB_CANCEL,
            None,
            json!({"jobId": "j_missing"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::JobNotFound));

    // Mutating kinds are refused before admission in this build.
    let err = c
        .call_raw(
            method::INDEX_REQUEST,
            Some(&pid),
            json!({"kind": "full"}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotImplemented));

    drop(c);
    daemon.stop(&root).await;

    // Durability: a new daemon instance serves the same registration and result.
    let daemon = Daemon::start(&root).await;
    let mut c = client(&root).await;
    let job: JobRecord = c
        .call(method::JOB_GET, None, &json!({"jobId": first.job.job_id}))
        .await
        .unwrap();
    assert_eq!(job, done);
    let list = c
        .call_raw(method::PROJECT_LIST, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(list["projects"][0]["projectId"], json!(pid));

    // Detach preserves the index.
    c.call_raw(method::PROJECT_DETACH, Some(&pid), json!({}), None)
        .await
        .unwrap();
    assert!(db.exists());
    assert_eq!(sha(&db), before);
    drop(c);
    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_starts_elect_exactly_one_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let mut children: Vec<Child> = (0..6).map(|_| Daemon::spawn(&root)).collect();
    wait_ready(&root).await;

    // Losers exit with code 3 without touching the registry or discovery.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let mut losers = 0;
        for child in children.iter_mut() {
            if let Some(status) = child.try_wait().unwrap() {
                assert_eq!(status.code(), Some(3), "unexpected exit {status}");
                losers += 1;
            }
        }
        if losers == 5 || Instant::now() >= deadline {
            assert_eq!(losers, 5, "expected five losing starters");
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut survivor = children
        .into_iter()
        .find_map(|mut c| c.try_wait().unwrap().is_none().then_some(c))
        .expect("exactly one running daemon");

    let discovery = Discovery::load(&data_root(&root)).unwrap();
    assert_eq!(discovery.info.pid, survivor.id());
    for _ in 0..5 {
        let c = client(&root).await;
        assert_eq!(
            c.hello().daemon_instance_id,
            discovery.info.daemon_instance_id
        );
    }

    let mut c = client(&root).await;
    c.call_raw(method::DAEMON_SHUTDOWN, None, json!({}), None)
        .await
        .unwrap();
    drop(c);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = survivor.try_wait().unwrap() {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(Instant::now() < deadline, "survivor did not stop");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!data_root(&root).endpoint_file().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn transport_rejects_bad_credentials_and_bounds_frames() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let daemon = Daemon::start(&root).await;
    let discovery = Discovery::load(&data_root(&root)).unwrap();
    let endpoint = discovery.info.endpoint.clone();

    // Wrong token: typed refusal, no hello.
    let err = Client::connect_with(&endpoint, &"0".repeat(64), "intruder")
        .await
        .err()
        .expect("must be rejected");
    assert!(
        matches!(&err, CallError::Remote(e) if e.code == ErrorCode::Unauthenticated),
        "{err}"
    );

    // Requests before authentication are treated as a failed auth frame.
    let mut raw = wrongindex_ipc::win::connect(&endpoint, Duration::from_secs(5))
        .await
        .unwrap();
    write_frame(
        &mut raw,
        &json!({"protocol": 1, "requestId": "x", "method": "project.list"}),
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let reply: AuthResponse = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(reply, AuthResponse::Error { ref error } if error.code == ErrorCode::Unauthenticated)
    );

    // Authenticated: malformed JSON is answered and the connection survives.
    let mut raw = wrongindex_ipc::win::connect(&endpoint, Duration::from_secs(5))
        .await
        .unwrap();
    write_frame(
        &mut raw,
        &AuthRequest {
            protocol_major: 1,
            protocol_minor: 0,
            token: discovery.token.clone(),
            client_name: "raw".into(),
            client_version: "0".into(),
            supported_profile_ids: vec![],
        },
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let hello: AuthResponse = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(hello, AuthResponse::Ok { .. }));
    raw.write_all(&3u32.to_le_bytes()).await.unwrap();
    raw.write_all(b"{{{").await.unwrap();
    let resp: Response = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.error.unwrap().code, ErrorCode::InvalidRequest);
    write_frame(
        &mut raw,
        &json!({"protocol": 1, "requestId": "after", "method": "daemon.hello"}),
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let resp: Response = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.request_id, "after");
    assert!(resp.result.is_some());

    // Unknown method is typed.
    write_frame(
        &mut raw,
        &json!({"protocol": 1, "requestId": "m", "method": "nope"}),
        MAX_FRAME_BYTES,
    )
    .await
    .unwrap();
    let resp: Response = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.error.unwrap().code, ErrorCode::MethodNotFound);

    // Oversized header: rejected before the body is read, then closed.
    raw.write_all(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes())
        .await
        .unwrap();
    let resp: Response = read_frame(&mut raw, MAX_FRAME_BYTES)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.error.unwrap().code, ErrorCode::InvalidRequest);
    let closed: Option<Value> = read_frame(&mut raw, MAX_FRAME_BYTES).await.unwrap_or(None);
    assert!(
        closed.is_none(),
        "connection must close after an oversized frame"
    );

    // Silent unauthenticated client is dropped after the auth deadline.
    let mut idle = wrongindex_ipc::win::connect(&endpoint, Duration::from_secs(5))
        .await
        .unwrap();
    let started = Instant::now();
    let eof: Option<Value> = tokio::time::timeout(
        Duration::from_secs(15),
        read_frame(&mut idle, MAX_FRAME_BYTES),
    )
    .await
    .expect("server must drop the idle client")
    .unwrap_or(None);
    assert!(eof.is_none());
    assert!(started.elapsed() >= Duration::from_secs(4));

    daemon.stop(&root).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn registration_identity_rules_over_ipc() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let index = tmp.path().join("idx");
    let daemon = Daemon::start(&root).await;
    let mut c = client(&root).await;

    let first: RegisterProjectResult = c
        .call(
            method::PROJECT_REGISTER,
            None,
            &json!({"checkoutRoot": a, "indexDir": index}),
        )
        .await
        .unwrap();
    // Alias of the same checkout (different case / trailing separator) is the same project.
    let alias = format!("{}\\", a.display().to_string().to_uppercase());
    let again: RegisterProjectResult = c
        .call(
            method::PROJECT_REGISTER,
            None,
            &json!({"checkoutRoot": alias, "indexDir": index}),
        )
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.project.project_id, first.project.project_id);

    let err = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": b, "indexDir": index}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::IndexIdentityConflict));

    let err = c
        .call_raw(
            method::PROJECT_REGISTER,
            None,
            json!({"checkoutRoot": tmp.path().join("missing"), "indexDir": tmp.path().join("i2")}),
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::ProjectUnavailable));

    // A check on a project without an index reports `missing`, not success-with-data.
    let admission: JobAdmission = serde_json::from_value(
        c.call_raw(
            method::INDEX_CHECK,
            Some(&first.project.project_id),
            json!({}),
            None,
        )
        .await
        .unwrap(),
    )
    .unwrap();
    let job = wait_job(&mut c, &admission.job.job_id).await;
    assert_eq!(job.result.unwrap()["verdict"], "missing");

    drop(c);
    daemon.stop(&root).await;
}

async fn next_event(s: &mut wrongindex_ipc::EventStream) -> Value {
    tokio::time::timeout(Duration::from_secs(10), s.next())
        .await
        .expect("event within 10 s")
        .unwrap()
        .expect("stream open")
}

#[tokio::test(flavor = "multi_thread")]
async fn event_stream_orders_replays_and_resynchronizes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("home");
    let checkout = tmp.path().join("checkout");
    std::fs::create_dir_all(&checkout).unwrap();
    let db = fixture_index("basic-ts", &tmp.path().join("index"));

    let daemon = Daemon::start(&root).await;
    let mut c = client(&root).await;
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    let instance = snap["instanceId"].as_str().unwrap().to_string();
    let start_seq = snap["seq"].as_u64().unwrap();
    assert_eq!(snap["projects"], json!([]));

    let (ack, mut live) = client(&root)
        .await
        .subscribe(Some(start_seq), Some(&instance), None)
        .await
        .unwrap();
    assert_eq!(ack["gap"], false);
    assert_eq!(ack["instanceId"], json!(instance));

    let reg: RegisterProjectResult = c
        .call(
            method::PROJECT_REGISTER,
            None,
            &json!({"checkoutRoot": checkout, "indexDir": db.parent().unwrap()}),
        )
        .await
        .unwrap();
    let pid = reg.project.project_id.clone();
    let job: JobAdmission = c
        .call(method::INDEX_CHECK, Some(&pid), &json!({}))
        .await
        .unwrap();

    // Registration, then the job lifecycle, strictly in sequence order.
    let mut seen = Vec::new();
    let mut last_seq = start_seq;
    while seen.last().map(String::as_str) != Some("job.finished") {
        let ev = next_event(&mut live).await;
        let seq = ev["seq"].as_u64().unwrap();
        assert_eq!(seq, last_seq + 1, "gapless, ordered sequence: {ev}");
        last_seq = seq;
        assert_eq!(ev["instanceId"], json!(instance));
        assert_eq!(ev["projectId"], json!(pid));
        if ev["kind"] == "job.finished" {
            assert_eq!(ev["data"]["jobId"], json!(job.job.job_id));
            assert_eq!(ev["data"]["state"], "succeeded");
        }
        seen.push(ev["kind"].as_str().unwrap().to_string());
    }
    assert_eq!(
        seen,
        [
            "project.registered",
            "job.queued",
            "job.started",
            "job.finished"
        ]
    );

    // A late subscriber replays retained history from its last position.
    let (ack, mut replay) = client(&root)
        .await
        .subscribe(Some(start_seq + 1), Some(&instance), Some(&pid))
        .await
        .unwrap();
    assert_eq!(ack["replayed"], 3);
    assert_eq!(next_event(&mut replay).await["kind"], "job.queued");

    // Another instance's position is meaningless: the client must resync.
    let (ack, _) = client(&root)
        .await
        .subscribe(Some(1), Some("d_other"), None)
        .await
        .unwrap();
    assert_eq!(ack["gap"], true);

    // The snapshot reflects the state the events described.
    let snap = c
        .call_raw(method::EVENTS_SNAPSHOT, None, json!({}), None)
        .await
        .unwrap();
    assert_eq!(snap["seq"].as_u64().unwrap(), last_seq);
    assert_eq!(snap["projects"][0]["project"]["projectId"], json!(pid));
    assert_eq!(snap["projects"][0]["lastJob"]["state"], "succeeded");

    // Shutdown announces draining, then closes the stream.
    drop(c);
    drop(replay);
    let stopping = tokio::spawn(async move { daemon.stop(&root).await });
    let mut kinds = Vec::new();
    while let Ok(Ok(Some(ev))) = tokio::time::timeout(Duration::from_secs(20), live.next()).await {
        kinds.push(ev["kind"].as_str().unwrap_or_default().to_string());
    }
    assert_eq!(kinds, ["daemon.draining"]);
    stopping.await.unwrap();
}
