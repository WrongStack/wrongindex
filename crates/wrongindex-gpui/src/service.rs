//! Bounded native client actor. GPUI never performs blocking IPC or SQLite work.
use crate::model::{Command, Preview, Snapshot};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::Duration;
use wrongindex_contracts::{ErrorCode, method};
use wrongindex_ipc::{CallError, Client, DataRoot};

pub struct Service {
    pub commands: SyncSender<(u64, Command)>,
    pub updates: Receiver<Snapshot>,
}

pub fn spawn(root: DataRoot, daemon: PathBuf) -> Service {
    let (commands, requests) = sync_channel(32);
    let (updates, received) = sync_channel(8);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(4)
            .enable_all()
            .build()
        {
            Ok(runtime) => runtime,
            Err(_) => return,
        };
        let mut state = Snapshot::default();
        let mut selected = None;
        loop {
            runtime.block_on(refresh(&root, &mut state, selected.as_deref()));
            if let Err(TrySendError::Disconnected(_)) = updates.try_send(state.clone()) {
                break;
            }
            match requests.recv_timeout(Duration::from_secs(1)) {
                Ok((sequence, command)) => {
                    if let Command::Select(id) = &command {
                        selected = Some(id.clone());
                    }
                    if let Err(error) =
                        runtime.block_on(handle(&root, &daemon, command, &mut state))
                    {
                        state.notice = Some(error);
                    }
                    state.acknowledged = sequence;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    });
    Service {
        commands,
        updates: received,
    }
}

async fn connect(root: &DataRoot) -> Result<Client, String> {
    tokio::time::timeout(
        Duration::from_secs(5),
        Client::connect(root, "wrongindex-gpui"),
    )
    .await
    .map_err(|_| "Connection timed out. The service may be stopped.".to_string())?
    .map_err(|e| match e {
        wrongindex_ipc::CallError::Unavailable(_) => {
            "The background service is unavailable. Start it to connect to your projects.".into()
        }
        other => other.to_string(),
    })
}

async fn request(
    client: &mut Client,
    method: &str,
    project: Option<&str>,
    params: Value,
) -> Result<Value, String> {
    tokio::time::timeout(
        Duration::from_secs(10),
        client.call_raw(method, project, params, None),
    )
    .await
    .map_err(|_| "Service request timed out; check its actual state before retrying.".to_string())?
    .map_err(|e| e.to_string())
}

async fn start(root: &DataRoot, daemon: &std::path::Path) -> Result<(), String> {
    tokio::time::timeout(Duration::from_secs(30), async {
        let mut child: Option<wrongindex_ipc::launch::Detached> = None;
        let mut last_spawn = tokio::time::Instant::now();
        loop {
            match Client::connect(root, "wrongindex-gpui").await {
                Ok(_) => return Ok(()),
                Err(error) if error.code() == Some(ErrorCode::ShuttingDown) => {
                    // The old instance retains the endpoint until its jobs
                    // finish. Do not mistake it for a ready service or spawn
                    // a replacement while it is still draining.
                }
                Err(CallError::Unavailable(_)) => {
                    let should_spawn = match child.as_mut() {
                        None => true,
                        Some(child) => match child.try_wait().map_err(|e| e.to_string())? {
                            None => false,
                            Some(3) => last_spawn.elapsed() >= Duration::from_millis(500),
                            Some(code) => {
                                return Err(format!(
                                    "Background service exited with code {code}; check its logs."
                                ));
                            }
                        },
                    };
                    if should_spawn {
                        child = Some(
                            wrongindex_ipc::launch::spawn_detached(daemon, root)
                                .map_err(|e| e.to_string())?,
                        );
                        last_spawn = tokio::time::Instant::now();
                    }
                }
                Err(error) => return Err(error.to_string()),
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| {
        "Service did not become ready within 30 seconds. It may still be shutting down.".to_string()
    })?
}

async fn refresh(root: &DataRoot, state: &mut Snapshot, selected: Option<&str>) {
    let result = async {
        let mut client = connect(root).await?;
        let health = request(&mut client, method::DAEMON_HEALTH, None, json!({})).await?;
        let snapshot = request(&mut client, method::EVENTS_SNAPSHOT, None, json!({})).await?;
        let metrics = request(&mut client, method::DAEMON_METRICS, None, json!({})).await?;
        let settings = request(&mut client, method::SETTINGS_GET, None, json!({})).await?;
        let jobs = request(&mut client, method::JOB_LIST, None, json!({"limit":20})).await?;
        let projects = serde_json::from_value(snapshot["projects"].clone())
            .map_err(|e| format!("Invalid project snapshot: {e}"))?;
        state.health = health;
        state.metrics = metrics;
        state.settings = settings;
        state.projects = projects;
        state.jobs = jobs["jobs"].as_array().cloned().unwrap_or_default();
        state.stats = Value::Null;
        state.stats_project = None;
        if let Some(id) = selected {
            match request(
                &mut client,
                method::INDEX_QUERY,
                Some(id),
                json!({"op":"stats","args":{}}),
            )
            .await
            {
                Ok(reply)
                    if reply["indexCorrupt"].is_null()
                        && reply["publicationPending"] != true
                        && state.projects.iter().any(|p| {
                            p.project.project_id == id
                                && Some(p.generation) == reply["generation"].as_i64()
                        }) =>
                {
                    state.stats = reply["result"].clone();
                    state.stats_project = Some(id.to_string());
                }
                Ok(_) => {
                    state.notice = Some(
                        "Index publication is pending or damaged; statistics are unavailable."
                            .into(),
                    )
                }
                Err(_) => {} // An unindexed/manual project legitimately has no statistics yet.
            }
        }
        Ok::<_, String>(())
    }
    .await;
    match result {
        Ok(()) => {
            state.connected = true;
            state.error = None;
        }
        Err(error) => {
            state.connected = false;
            state.error = Some(error);
            state.projects.clear();
            state.jobs.clear();
            state.stats = Value::Null;
            state.stats_project = None;
        }
    }
}

async fn handle(
    root: &DataRoot,
    daemon: &std::path::Path,
    command: Command,
    state: &mut Snapshot,
) -> Result<(), String> {
    match command {
        Command::Start => {
            start(root, daemon).await?;
            state.notice = Some("Background service is ready.".into());
            return Ok(());
        }
        Command::Preview(path) => {
            let preview = tokio::task::spawn_blocking(move || {
                let global = wrongindex_storage::wstack_global_root()
                    .ok_or("Cannot resolve the WrongStack data root")?;
                let resolved = wrongindex_storage::resolve_index_dir(&path, &global);
                let inspection = wrongindex_storage::inspect_index(
                    &PathBuf::from(&resolved.index_dir).join("index.db"),
                );
                let value =
                    serde_json::to_value(inspection).map_err(|_| "Cannot inspect the index")?;
                Ok::<_, &str>(Preview {
                    root: path,
                    index_dir: resolved.index_dir,
                    verdict: value["verdict"].as_str().unwrap_or("unknown").to_string(),
                })
            })
            .await
            .map_err(|_| "Preview task failed".to_string())?
            .map_err(str::to_string)?;
            state.preview = Some(preview);
            state.notice = None;
            return Ok(());
        }
        Command::DismissPreview => {
            state.preview = None;
            return Ok(());
        }
        Command::Refresh | Command::Select(_) => return Ok(()),
        _ => {}
    }
    let mut client = connect(root).await?;
    let response = match command {
        Command::Register {
            root,
            index_dir,
            pinned,
        } => {
            let reply = request(&mut client, method::PROJECT_REGISTER, None, json!({"checkoutRoot":root,"indexDir":index_dir,"watchPolicy":if pinned {"pinned"} else {"manual"}})).await?;
            state.preview = None;
            reply
        }
        Command::Index { project, kind } => {
            if kind != "check" {
                request(
                    &mut client,
                    method::OWNERSHIP_ACQUIRE,
                    Some(&project),
                    json!({}),
                )
                .await?;
            }
            request(
                &mut client,
                method::INDEX_REQUEST,
                Some(&project),
                json!({"kind":kind}),
            )
            .await?
        }
        Command::Policy { project, policy } => {
            request(
                &mut client,
                method::PROJECT_CONFIGURE,
                Some(&project),
                json!({"watchPolicy":policy}),
            )
            .await?
        }
        Command::Pause(paused) => {
            request(
                &mut client,
                method::SETTINGS_SET,
                None,
                json!({"settings":{"paused":paused}}),
            )
            .await?
        }
        _ => return Ok(()),
    };
    state.notice = Some(if let Some(job) = response["job"]["jobId"].as_str() {
        format!("Job {job} accepted. Its completion is shown in Activity.")
    } else {
        "Change acknowledged by the service.".into()
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::{Child, Stdio};
    use std::time::Instant;

    struct Daemon(Child);
    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn daemon_path() -> PathBuf {
        PathBuf::from(std::env::var_os("WRONGINDEX_TEST_DAEMON").expect("WRONGINDEX_TEST_DAEMON"))
    }

    fn next(service: &Service, predicate: impl Fn(&Snapshot) -> bool) -> Snapshot {
        let deadline = Instant::now() + Duration::from_secs(45);
        loop {
            if let Ok(snapshot) = service.updates.recv_timeout(Duration::from_millis(100))
                && predicate(&snapshot)
            {
                return snapshot;
            }
            assert!(Instant::now() < deadline, "GPUI actor did not converge");
        }
    }

    fn send(service: &Service, sequence: u64, command: Command) -> Snapshot {
        assert!(service.commands.try_send((sequence, command)).is_ok());
        next(service, |s| s.acknowledged >= sequence)
    }

    fn write(root: &Path, name: &str) {
        std::fs::write(
            root.join("entry.ts"),
            format!("export function {name}() {{ return 1; }}\n"),
        )
        .unwrap();
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real GPUI client actor and daemon"]
    fn actor_registers_indexes_and_closing_the_client_preserves_pinned_watching() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let index = temp.path().join("index");
        std::fs::create_dir(&project).unwrap();
        write(&project, "beforeClientClose");
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let daemon = daemon_path();
        let _daemon = Daemon(
            std::process::Command::new(&daemon)
                .arg("--data-root")
                .arg(root.path())
                .arg("--backend")
                .arg("native")
                .env_remove("WRONGINDEX_ENGINE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let service = spawn(root.clone(), daemon.clone());
        next(&service, |s| s.connected);
        let registered = send(
            &service,
            1,
            Command::Register {
                root: project.clone(),
                index_dir: index.display().to_string(),
                pinned: true,
            },
        );
        assert!(
            registered
                .notice
                .as_deref()
                .unwrap()
                .contains("acknowledged")
        );
        let current = next(&service, |s| {
            s.projects.first().is_some_and(|p| {
                p.freshness == wrongindex_contracts::Freshness::Current && p.generation > 0
            })
        });
        let id = current.projects[0].project.project_id.clone();
        send(&service, 2, Command::Select(id.clone()));
        let counts = next(&service, |s| {
            s.stats_project.as_deref() == Some(id.as_str())
        });
        assert_eq!(counts.stats["totalFiles"], 1, "{}", counts.stats);
        let checked = send(
            &service,
            3,
            Command::Index {
                project: id.clone(),
                kind: "check",
            },
        );
        assert!(checked.notice.as_deref().unwrap().contains("accepted"));
        drop(service);
        write(&project, "afterClientClose");
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut client = connect(&root).await.unwrap();
            let deadline = Instant::now() + Duration::from_secs(30);
            loop {
                let result = request(
                    &mut client,
                    method::INDEX_QUERY,
                    Some(&id),
                    json!({"op":"search","args":{"query":"afterClientClose","limit":5}}),
                )
                .await
                .unwrap();
                if result["result"].to_string().contains("afterClientClose") {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "pinned watcher stopped after GPUI client drop"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
        // Registration and data survive the client; only this test's daemon
        // and disposable roots are removed by their guards.
        assert!(index.join("index.db").exists());
    }

    #[test]
    #[ignore = "needs Windows IPC; real offline GPUI client actor"]
    fn offline_actor_reports_errors_without_inventing_projects_or_writing_an_index() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let service = spawn(root, temp.path().join("missing-daemon.exe"));
        let offline = next(&service, |s| s.error.is_some());
        assert!(!offline.connected);
        assert!(offline.projects.is_empty());
        let failed = send(&service, 1, Command::Start);
        assert!(!failed.connected);
        assert!(failed.notice.is_some());
        assert!(failed.projects.is_empty());
        assert!(!temp.path().join("index.db").exists());
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real settings mutation through the GPUI actor"]
    fn pause_and_resume_are_acknowledged_and_preserve_other_settings() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let daemon = daemon_path();
        let _daemon = Daemon(
            std::process::Command::new(&daemon)
                .arg("--data-root")
                .arg(root.path())
                .env_remove("WRONGINDEX_ENGINE")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let service = spawn(root.clone(), daemon);
        next(&service, |s| s.connected);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let mut client = connect(&root).await.unwrap();
            request(
                &mut client,
                method::SETTINGS_SET,
                None,
                json!({"settings":{"reconcileMinutes":17}}),
            )
            .await
            .unwrap();
        });
        for (sequence, paused) in [(1, true), (2, false)] {
            let acknowledged = send(&service, sequence, Command::Pause(paused));
            assert_eq!(
                acknowledged.notice.as_deref(),
                Some("Change acknowledged by the service."),
                "pause={paused}: {:?}",
                acknowledged.notice
            );
            assert_eq!(acknowledged.settings["settings"]["paused"], paused);
            runtime.block_on(async {
                let mut client = connect(&root).await.unwrap();
                let actual = request(&mut client, method::SETTINGS_GET, None, json!({}))
                    .await
                    .unwrap();
                assert_eq!(actual["settings"]["paused"], paused);
                assert_eq!(actual["settings"]["reconcileMinutes"], 17);
                assert_eq!(actual["revision"], sequence + 1);
            });
        }
    }

    #[test]
    #[ignore = "needs WRONGINDEX_TEST_DAEMON; real cold start and already-running GPUI client"]
    fn actor_start_authenticates_and_preserves_an_already_running_instance() {
        let temp = tempfile::tempdir().unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let service = spawn(root.clone(), daemon_path());
        next(&service, |s| !s.connected && s.error.is_some());
        let started = send(&service, 1, Command::Start);
        let repeated = send(&service, 2, Command::Start);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let stopped = runtime.block_on(async {
            let mut client = connect(&root).await?;
            request(&mut client, method::DAEMON_SHUTDOWN, None, json!({})).await
        });
        drop(service);
        stopped.unwrap();
        assert!(
            started.connected,
            "Start must acknowledge a real authenticated service"
        );
        assert!(repeated.connected);
        assert_eq!(
            started.health["daemonInstanceId"],
            repeated.health["daemonInstanceId"]
        );
        assert_eq!(
            started.notice.as_deref(),
            Some("Background service is ready.")
        );
        assert_eq!(
            repeated.notice.as_deref(),
            Some("Background service is ready.")
        );
    }

    #[test]
    #[ignore = "needs a debug WRONGINDEX_TEST_DAEMON with the native stall hook"]
    fn debug_start_waits_for_draining_service_and_preserves_its_job() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("slowproj");
        std::fs::create_dir(&project).unwrap();
        std::fs::write(project.join("config.json"), "{\"keepAfterRestart\":true}").unwrap();
        let root = DataRoot::resolve(Some(&temp.path().join("service"))).unwrap();
        let daemon = daemon_path();
        let _first = Daemon(
            std::process::Command::new(&daemon)
                .arg("--data-root")
                .arg(root.path())
                .args(["--drain-seconds", "60"])
                .env_remove("WRONGINDEX_ENGINE")
                .env("WRONGINDEX_NATIVE_STALL", "slowproj:8")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let service = spawn(root.clone(), daemon);
        next(&service, |s| s.connected);
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let (old_instance, job_id) = runtime.block_on(async {
            let mut client = connect(&root).await.unwrap();
            let old_instance = client.hello().daemon_instance_id.clone();
            let reg = request(&mut client, method::PROJECT_REGISTER, None, json!({"checkoutRoot":project,"indexDir":temp.path().join("index"),"watchPolicy":"manual"})).await.unwrap();
            let id = reg["project"]["projectId"].as_str().unwrap();
            request(&mut client, method::OWNERSHIP_ACQUIRE, Some(id), json!({})).await.unwrap();
            let admitted = request(&mut client, method::INDEX_REQUEST, Some(id), json!({"kind":"full"})).await.unwrap();
            let job_id = admitted["job"]["jobId"].as_str().unwrap().to_string();
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                let job = request(&mut client, method::JOB_GET, None, json!({"jobId":job_id})).await.unwrap();
                if job["state"] == "running" { break; }
                assert!(Instant::now() < deadline, "fixture job did not start");
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            request(&mut client, method::DAEMON_SHUTDOWN, None, json!({})).await.unwrap();
            let refused = Client::connect(&root, "drain-precondition").await;
            assert_eq!(refused.err().and_then(|e| e.code()), Some(wrongindex_contracts::ErrorCode::ShuttingDown), "fixture must be draining when Start is pressed");
            (old_instance, job_id)
        });
        let acknowledged = send(&service, 1, Command::Start);
        // Always stop a reachable successor before asserting, including on a
        // failed assertion. Only the original child has a process guard.
        let successor = runtime.block_on(async {
            let Ok(mut client) = connect(&root).await else {
                return None;
            };
            let instance = client.hello().daemon_instance_id.clone();
            let job = request(&mut client, method::JOB_GET, None, json!({"jobId":job_id})).await;
            let stopped = request(&mut client, method::DAEMON_SHUTDOWN, None, json!({})).await;
            Some((instance, job, stopped))
        });
        drop(service);
        assert!(
            acknowledged.connected,
            "Start was acknowledged before an authenticated successor was ready: {:?}",
            acknowledged.notice
        );
        let (instance, job, stopped) =
            successor.expect("Start must launch an authenticated successor after the drain");
        assert_ne!(instance, old_instance);
        assert_eq!(
            job.unwrap()["state"],
            "succeeded",
            "Start must let the old job drain instead of killing it"
        );
        stopped.unwrap();
    }
}
