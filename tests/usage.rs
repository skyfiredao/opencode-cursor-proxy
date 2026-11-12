use cursor_proxy::usage::read_checkpoint;
use rusqlite::Connection;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use uuid::Uuid;

struct Fixture {
    root: PathBuf,
    session: String,
    database: PathBuf,
}

impl Fixture {
    fn replace(&self, data: &[u8]) {
        let connection = Connection::open(&self.database).expect("writer");
        let id = format!("{:x}", Sha256::digest(data));
        let metadata = json!({"latestRootBlobId":id})
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        connection
            .execute("UPDATE meta SET value=?1 WHERE key='0'", [metadata])
            .expect("root");
        connection
            .execute(
                "INSERT OR REPLACE INTO blobs VALUES(?1,?2)",
                rusqlite::params![id, data],
            )
            .expect("blob");
    }

    fn new() -> Self {
        let root = std::env::temp_dir().join(format!("cursor-usage-{}", Uuid::new_v4()));
        let session = Uuid::new_v4().to_string();
        let directory = root.join("acp-sessions").join(&session);
        std::fs::create_dir_all(&directory).expect("fixture directory");
        let database = directory.join("store.db");
        let connection = Connection::open(&database).expect("database");
        connection.execute_batch("PRAGMA user_version=1; CREATE TABLE meta(key TEXT PRIMARY KEY,value TEXT); CREATE TABLE blobs(id TEXT PRIMARY KEY,data BLOB);").expect("schema");
        let data = [0x2a, 0x07, 0x08, 0x87, 0x5f, 0x10, 0xc0, 0x9a, 0x0c];
        let id = format!("{:x}", Sha256::digest(data));
        let metadata = json!({"latestRootBlobId":id,"ignored":"do-not-export"})
            .to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        connection
            .execute("INSERT INTO meta VALUES('0',?1)", [metadata])
            .expect("metadata");
        connection
            .execute(
                "INSERT INTO blobs VALUES(?1,?2)",
                rusqlite::params![id, data.as_slice()],
            )
            .expect("blob");
        Self {
            root,
            session,
            database,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.root).expect("fixture cleanup");
    }
}

#[test]
fn cancelled_or_timed_out_publication_cannot_write_after_its_queued_worker_starts() {
    for abort in [false, true] {
        let fixture = Fixture::new();
        let path = fixture.root.join("opencode.jsonc");
        let source = "{\"providers\":{\"cursor-proxy\":{\"models\":{\"composer-2.5\":{}}}}}";
        std::fs::write(&path, source).expect("configuration");
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(1)
            .enable_time()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let (started, ready) = std::sync::mpsc::channel();
            let (release, held) = std::sync::mpsc::channel();
            let blocker = tokio::task::spawn_blocking(move || {
                started.send(()).expect("ready");
                held.recv().expect("release");
            });
            ready.recv().expect("blocking pool occupied");
            let notified = std::sync::Arc::new(tokio::sync::Notify::new());
            let signal = notified.clone();
            let path = path.clone();
            let publication = tokio::spawn(async move {
                signal.notify_one();
                cursor_proxy::usage::publish_capacity(
                    cursor_proxy::opencode::Version::V2,
                    path,
                    "composer-2.5".to_owned(),
                    100000,
                )
                .await;
            });
            notified.notified().await;
            if abort {
                publication.abort();
                assert!(publication.await.expect_err("cancelled").is_cancelled());
            } else {
                tokio::time::timeout(std::time::Duration::from_millis(500), publication)
                    .await
                    .expect("bounded observation")
                    .expect("timeout handled");
            }
            release.send(()).expect("release blocker");
            blocker.await.expect("blocking task");
        });
        runtime.shutdown_timeout(std::time::Duration::from_secs(1));
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), source);
    }
}

#[test]
fn own_session_checkpoint_is_read_without_changing_database_or_creating_other_sessions() {
    let fixture = Fixture::new();
    let before = std::fs::read(&fixture.database).expect("database bytes");
    let snapshot = read_checkpoint(&fixture.root, &fixture.session)
        .expect("read-only query")
        .expect("context");
    assert_eq!(snapshot.used, 12167);
    assert_eq!(snapshot.capacity, 200000);
    assert_eq!(
        before,
        std::fs::read(&fixture.database).expect("unchanged database")
    );
    assert!(
        read_checkpoint(&fixture.root, &Uuid::new_v4().to_string())
            .expect("missing")
            .is_none()
    );
    assert_eq!(
        std::fs::read_dir(fixture.root.join("acp-sessions"))
            .expect("sessions")
            .count(),
        1
    );
}

#[test]
fn a_new_cursor_store_with_an_empty_root_has_no_checkpoint_yet() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.database).expect("writer");
    let metadata = json!({"latestRootBlobId":""})
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    connection
        .execute("UPDATE meta SET value=?1 WHERE key='0'", [metadata])
        .expect("empty root");
    assert!(
        read_checkpoint(&fixture.root, &fixture.session)
            .expect("valid new store")
            .is_none()
    );
}

#[test]
fn malformed_or_unavailable_context_never_becomes_a_counter() {
    let fixture = Fixture::new();
    for data in [
        vec![0],
        vec![0x28, 1],
        vec![0x2a, 10, 8],
        vec![0x2b],
        vec![0x2a, 0, 0x2a, 0],
        vec![0x2a, 6, 8, 1, 8, 2, 16, 16],
        vec![
            0x2a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
        ],
    ] {
        fixture.replace(&data);
        assert!(
            read_checkpoint(&fixture.root, &fixture.session).is_err(),
            "{data:?}"
        );
    }
    for data in [vec![], vec![0x2a, 0], vec![0x2a, 2, 8, 1]] {
        fixture.replace(&data);
        assert!(
            read_checkpoint(&fixture.root, &fixture.session)
                .expect("absent capacity")
                .is_none()
        );
    }
    fixture.replace(&[0x2a, 4, 0x10, 0xc0, 0x9a, 0x0c]);
    assert_eq!(
        read_checkpoint(&fixture.root, &fixture.session)
            .expect("zero")
            .expect("context")
            .used,
        0
    );
    assert!(read_checkpoint(&fixture.root, "../escape").is_err());
    let connection = Connection::open(&fixture.database).expect("writer");
    connection
        .execute_batch("PRAGMA user_version=2")
        .expect("future schema");
    assert!(read_checkpoint(&fixture.root, &fixture.session).is_err());
}

#[test]
fn wal_snapshot_is_visible_and_lock_contention_is_bounded() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.database).expect("writer");
    connection
        .execute_batch("PRAGMA journal_mode=WAL")
        .expect("WAL");
    let data = [0x2a, 4, 8, 100, 16, 127];
    let id = format!("{:x}", Sha256::digest(data));
    let metadata = json!({"latestRootBlobId":id})
        .to_string()
        .bytes()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    connection
        .execute(
            "INSERT INTO blobs VALUES(?1,?2)",
            rusqlite::params![id, data.as_slice()],
        )
        .expect("WAL blob");
    connection
        .execute("UPDATE meta SET value=?1 WHERE key='0'", [metadata])
        .expect("WAL root");
    let before = std::fs::read(fixture.database.with_file_name("store.db-wal")).expect("WAL bytes");
    let snapshot = read_checkpoint(&fixture.root, &fixture.session)
        .expect("WAL read")
        .expect("context");
    assert_eq!((snapshot.used, snapshot.capacity), (100, 127));
    assert_eq!(
        before,
        std::fs::read(fixture.database.with_file_name("store.db-wal")).expect("unchanged WAL")
    );
    drop(connection);
    let connection = Connection::open(&fixture.database).expect("writer");
    connection
        .execute_batch("PRAGMA journal_mode=DELETE; BEGIN EXCLUSIVE")
        .expect("exclusive lock");
    let started = std::time::Instant::now();
    assert!(read_checkpoint(&fixture.root, &fixture.session).is_err());
    assert!(started.elapsed() < std::time::Duration::from_millis(250));
}

#[test]
fn oversized_values_and_escaping_symlinks_are_not_read() {
    let fixture = Fixture::new();
    let connection = Connection::open(&fixture.database).expect("writer");
    connection
        .execute(
            "UPDATE meta SET value=?1 WHERE key='0'",
            ["aa".repeat(32769)],
        )
        .expect("oversized metadata");
    assert!(
        read_checkpoint(&fixture.root, &fixture.session)
            .expect("capped metadata")
            .is_none()
    );
    fixture.replace(&vec![0; 2 * 1024 * 1024 + 1]);
    assert!(!matches!(
        read_checkpoint(&fixture.root, &fixture.session),
        Ok(Some(_))
    ));
    #[cfg(unix)]
    {
        drop(connection);
        let moved = fixture.root.join("outside.db");
        std::fs::rename(&fixture.database, &moved).expect("move fixture");
        std::os::unix::fs::symlink(&moved, &fixture.database).expect("symlink");
        assert!(read_checkpoint(&fixture.root, &fixture.session).is_err());
    }
}

#[cfg(unix)]
#[tokio::test]
async fn original_checkpoint_counts_reach_standard_json_and_sse_but_stale_roots_do_not() {
    use std::{
        process::{Command, Stdio},
        time::Duration,
    };
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            self.0.kill().expect("stop proxy");
            self.0.wait().expect("reap proxy");
        }
    }
    for (scenario, present) in [
        ("usage-store", true),
        ("usage-store-stale", false),
        ("usage-store-stale-second", true),
        ("usage-store-missing", false),
        ("usage-store-corrupt", false),
        ("usage-store-baseline-locked", false),
    ] {
        for streaming in [false, true] {
            let fixture = Fixture::new();
            let shim = fixture.root.join("opencode");
            std::fs::write(&shim, "#!/bin/sh\necho 2.2.7\n").expect("OpenCode version fixture");
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700))
                .expect("fixture mode");
            std::fs::write(fixture.root.join("acp-scenario"), scenario).expect("scenario");
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
            let port = reservation.local_addr().expect("address").port();
            drop(reservation);
            let _server = Server(
                Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
                    .arg("--workspace")
                    .arg(&fixture.root)
                    .args(["--port", &port.to_string(), "--cursor-agent"])
                    .arg(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/fixtures/acp-agent.py"),
                    )
                    .env("CURSOR_CONFIG_DIR", &fixture.root)
                    .env("OPENCODE_CONFIG_DIR", fixture.root.join("profile"))
                    .env(
                        "PATH",
                        std::env::join_paths(std::iter::once(fixture.root.clone()).chain(
                            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
                        ))
                        .expect("fixture PATH"),
                    )
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("proxy"),
            );
            let client = reqwest::Client::builder()
                .timeout(Duration::from_secs(5))
                .build()
                .expect("client");
            let url = format!("http://127.0.0.1:{port}");
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    if client.get(format!("{url}/health")).send().await.is_ok()
                        && fixture.root.join("profile/opencode.jsonc").is_file()
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("ready");
            let mut request = json!({"model":"composer-2.5","stream":streaming,"messages":[{"role":"user","content":"PONG"}]});
            for turn in 0..if matches!(scenario, "usage-store" | "usage-store-stale-second") {
                2
            } else {
                1
            } {
                let present = present && !(scenario == "usage-store-stale-second" && turn == 1);
                let output = client
                    .post(format!("{url}/v1/chat/completions"))
                    .json(&request)
                    .send()
                    .await
                    .expect("reply")
                    .error_for_status()
                    .expect("status")
                    .text()
                    .await
                    .expect("body");
                let bodies: Vec<serde_json::Value> = if streaming {
                    output
                        .lines()
                        .filter_map(|line| line.strip_prefix("data: "))
                        .filter(|data| *data != "[DONE]")
                        .map(|data| serde_json::from_str(data).expect("SSE JSON"))
                        .collect()
                } else {
                    vec![serde_json::from_str(&output).expect("JSON")]
                };
                let usages: Vec<_> = bodies.iter().filter_map(|body| body.get("usage")).collect();
                assert_eq!(usages.len(), usize::from(present), "{scenario}: {output}");
                if present {
                    assert_eq!(usages[0]["prompt_tokens"], 12167);
                    assert_eq!(usages[0]["total_tokens"], 12167);
                    assert_eq!(usages[0]["cursor_proxy"]["source"], "context_snapshot");
                    assert_eq!(usages[0]["cursor_proxy"]["context_window"], 100000);
                    let configuration =
                        std::fs::read_to_string(fixture.root.join("profile/opencode.jsonc"))
                            .expect("automatic configuration");
                    let configuration =
                        cursor_proxy::config::parse(&configuration).expect("configuration JSONC");
                    assert_eq!(
                        configuration["providers"]["cursor-proxy"]["models"]["composer-2.5"]["limit"]
                            ["context"],
                        100000
                    );
                }
                request["messages"]
                    .as_array_mut()
                    .expect("history")
                    .extend([
                        json!({"role":"assistant","content":"PONG"}),
                        json!({"role":"user","content":"PONG_AGAIN"}),
                    ]);
            }
        }
    }
}
