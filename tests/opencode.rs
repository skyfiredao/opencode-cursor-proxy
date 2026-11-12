use cursor_proxy::opencode::Version;

#[test]
fn version_selection_uses_the_major_not_one_specific_release() {
    for output in [
        "1.0.0",
        "1.18.16\r\n",
        "1.99.123",
        "v1.7.2-beta.3",
        "opencode 1.24.0+build.9",
    ] {
        assert_eq!(Version::parse(output), Ok(Version::V1), "{output}");
    }
    for output in [
        "2.0.0",
        "2.2.7\n",
        "v2.19.1",
        "opencode v2.2.7",
        "opencode 2.4.0-rc.2",
    ] {
        assert_eq!(Version::parse(output), Ok(Version::V2), "{output}");
    }
    for output in [
        "",
        "dev",
        "3.0.0",
        "11.18.16",
        "1.18",
        "1.18.x",
        "1.18.16\n2.0.0",
        "banner 1.18.16",
        "1.18.16-",
        "1.18.16+",
        "1.18.16;cmd",
    ] {
        assert!(Version::parse(output).is_err(), "{output}");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn version_probe_runs_the_cli_with_bounded_failure_and_cleans_up() {
    use std::{os::unix::fs::PermissionsExt, time::Duration};
    let root = std::env::temp_dir().join(format!("opencode-version-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir(&root).expect("fixture");
    let binary = root.join("opencode");
    for (script, expected) in [
        (
            "test \"$#\" = 1 && test \"$1\" = --version || exit 7; echo 1.4.27",
            Some(Version::V1),
        ),
        ("echo 2.2.7", Some(Version::V2)),
        ("echo 1.18.16; exit 1", None),
        ("yes x | head -c 20000", None),
        ("echo $$ > \"$0.pid\"; exec sleep 30", None),
    ] {
        std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).expect("script");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).expect("mode");
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            cursor_proxy::opencode::detect(&binary),
        )
        .await
        .expect("bounded probe");
        match expected {
            Some(version) => assert_eq!(result, Ok(version)),
            None => assert!(result.is_err()),
        }
    }
    let pid: i32 = std::fs::read_to_string(binary.with_extension("pid"))
        .expect("probe PID")
        .trim()
        .parse()
        .expect("PID");
    tokio::time::timeout(Duration::from_secs(1), async {
        while nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("timed-out probe killed");
    assert!(
        cursor_proxy::opencode::detect(&root.join("missing"))
            .await
            .is_err()
    );
    std::fs::remove_dir_all(&root).expect("cleanup");
}

#[cfg(unix)]
#[tokio::test]
async fn proxy_auto_selects_native_catalog_and_context_paths_for_v1_and_v2() {
    use serde_json::{Value, json};
    use std::{
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        process::{Command, Stdio},
        time::Duration,
    };
    struct Server(std::process::Child);
    impl Drop for Server {
        fn drop(&mut self) {
            if let Err(error) = self.0.kill() {
                eprintln!("proxy cleanup: {error}");
            }
            self.0.wait().expect("reap proxy");
        }
    }
    for (reported, version) in [
        ("1.0.0", Version::V1),
        ("1.18.16", Version::V1),
        ("1.99.4-rc.2", Version::V1),
        ("2.0.0", Version::V2),
        ("2.2.7", Version::V2),
        ("dev", Version::V2),
        ("3.0.0", Version::V2),
    ] {
        for stream in [false, true] {
            let root = std::env::temp_dir().join(format!("proxy-version-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(root.join("bin")).expect("fixture");
            std::fs::create_dir(root.join("profile")).expect("profile");
            let shim = root.join("bin/opencode");
            std::fs::write(
                &shim,
                format!("#!/bin/sh\ntest \"$1\" = --version || exit 7\necho '{reported}'\n"),
            )
            .expect("OpenCode shim");
            std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o700)).expect("mode");
            std::fs::write(root.join("acp-scenario"), "usage-store").expect("scenario");
            let profile = root.join("profile/opencode.jsonc");
            std::fs::write(&profile,"{\n// preserve\n\"model\":\"other/current\",\"provider\":{\"other\":{\"name\":\"Keep V1\"}},\"providers\":{\"other\":{\"name\":\"Keep V2\"}}}\n").expect("config");
            let reservation = std::net::TcpListener::bind("127.0.0.1:0").expect("port");
            let port = reservation.local_addr().expect("address").port();
            drop(reservation);
            let mut paths = vec![root.join("bin")];
            paths.extend(std::env::split_paths(
                &std::env::var_os("PATH").unwrap_or_default(),
            ));
            let server = Server(
                Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
                    .args(["--workspace"])
                    .arg(&root)
                    .args([
                        "--port",
                        &port.to_string(),
                        "--password",
                        "fixture-password",
                        "--cursor-agent",
                    ])
                    .arg(
                        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                            .join("tests/fixtures/acp-agent.py"),
                    )
                    .env("PATH", std::env::join_paths(paths).expect("PATH"))
                    .env("CURSOR_CONFIG_DIR", &root)
                    .env("OPENCODE_CONFIG_DIR", root.join("profile"))
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
                        && std::fs::read_to_string(&profile)
                            .expect("config")
                            .contains("Cursor Proxy")
                    {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("ready");
            let config =
                cursor_proxy::config::parse(&std::fs::read_to_string(&profile).expect("config"))
                    .expect("JSONC");
            let definition = &config[version.providers()]["cursor-proxy"];
            assert_eq!(
                definition[version.settings()]["apiKey"],
                "fixture-password",
                "{reported}: {config}"
            );
            assert!(
                config[version.other().providers()]
                    .get("cursor-proxy")
                    .is_none(),
                "{reported}: wrong target {config}"
            );
            let catalog: Value = tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let catalog: Value = client
                        .get(format!("{url}/v1/models"))
                        .bearer_auth("fixture-password")
                        .send()
                        .await
                        .expect("models")
                        .error_for_status()
                        .expect("status")
                        .json()
                        .await
                        .expect("JSON");
                    if catalog["data"].as_array().expect("models").len() == 1 {
                        break catalog;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("published catalog");
            assert_eq!(catalog["data"][0]["id"], "composer-2.5");
            let reply=client.post(format!("{url}/v1/chat/completions")).bearer_auth("fixture-password").json(&json!({"model":"composer-2.5","stream":stream,"messages":[{"role":"user","content":"PONG"}]})).send().await.expect("chat").error_for_status().expect("status").text().await.expect("body");
            assert!(reply.contains("PONG") && reply.contains("12167"), "{reply}");
            if stream {
                assert_eq!(reply.matches("[DONE]").count(), 1);
            }
            let text = std::fs::read_to_string(&profile).expect("config");
            let config = cursor_proxy::config::parse(&text).expect("JSONC");
            let limit =
                &config[version.providers()]["cursor-proxy"]["models"]["composer-2.5"]["limit"];
            assert_eq!(limit["context"], 100000);
            if version == Version::V1 {
                assert_eq!(limit["output"], 0);
            }
            assert!(text.contains("// preserve"));
            assert_eq!(config["model"], "other/current");
            assert_eq!(config["provider"]["other"]["name"], "Keep V1");
            assert_eq!(config["providers"]["other"]["name"], "Keep V2");
            drop(server);
            std::fs::remove_dir_all(&root).expect("cleanup");
        }
    }
}
