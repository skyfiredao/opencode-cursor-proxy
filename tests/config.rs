use cursor_proxy::{
    config,
    models::{self, Model},
};
use serde_json::json;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static SEQUENCE: AtomicU64 = AtomicU64::new(1);

fn inventory() -> Vec<Model> {
    vec![
        Model {
            id: "composer-2.5".to_owned(),
            name: "Composer 2.5".to_owned(),
        },
        Model {
            id: "other-model".to_owned(),
            name: "Other Model".to_owned(),
        },
    ]
}

struct Directory(PathBuf);

impl Directory {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "cursor-proxy-config-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).expect("test directory");
        Self(path)
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        for entry in std::fs::read_dir(&self.0).expect("test files") {
            if let Err(error) = std::fs::remove_file(entry.expect("test entry").path()) {
                eprintln!("config test cleanup: {error}");
            }
        }
        if let Err(error) = std::fs::remove_dir(&self.0) {
            eprintln!("config test directory cleanup: {error}");
        }
    }
}

#[test]
fn only_v1_native_zero_context_is_fillable_and_incomplete_prices_never_write() {
    use cursor_proxy::opencode::Version;
    for (version, context, changed) in [
        (Version::V1, 0, true),
        (Version::V1, 99999, false),
        (Version::V2, 0, false),
        (Version::V2, 99999, false),
    ] {
        let directory = Directory::new();
        let path = directory.0.join("opencode.jsonc");
        std::fs::write(&path,json!({(version.providers()):{"cursor-proxy":{"models":{"composer-2.5":{"limit":{"context":context,"output":8192}}}}}}).to_string()).expect("source");
        assert_eq!(
            config::publish_context_limit_for(
                version,
                &path,
                "composer-2.5",
                200000,
                &tokio_util::sync::CancellationToken::new()
            )
            .expect("capacity"),
            changed
        );
        let parsed =
            config::parse(&std::fs::read_to_string(&path).expect("output")).expect("JSONC");
        assert_eq!(
            parsed[version.providers()]["cursor-proxy"]["models"]["composer-2.5"]["limit"]["context"],
            if changed { 200000 } else { context }
        );
    }
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source =
        json!({"providers":{"cursor-proxy":{"models":{"composer-2.5":{"cost":{"input":7}}}}}})
            .to_string();
    std::fs::write(&path, &source).expect("source");
    assert!(config::configure_for(Version::V1, &path, None, None, &inventory()).is_err());
    assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), source);
    assert_eq!(std::fs::read_dir(&directory.0).expect("files").count(), 1);
}

#[test]
fn v1_unknown_context_zero_does_not_block_a_later_observation() {
    use cursor_proxy::opencode::Version;
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    std::fs::write(
        &path,
        json!({"providers":{"cursor-proxy":{"models":{"composer-2.5":{"limit":{"output":8192}}}}}})
            .to_string(),
    )
    .expect("output-only V2 override");
    config::configure_for(Version::V1, &path, None, None, &inventory()).expect("V1 publication");
    let before = config::parse(&std::fs::read_to_string(&path).expect("config")).expect("JSONC");
    assert_eq!(
        before["provider"]["cursor-proxy"]["models"]["composer-2.5"]["limit"],
        json!({"context":0,"output":8192})
    );
    let cancelled = tokio_util::sync::CancellationToken::new();
    assert!(
        config::publish_context_limit_for(Version::V1, &path, "composer-2.5", 100000, &cancelled)
            .expect("fill unknown V1 context")
    );
    let after = config::parse(&std::fs::read_to_string(&path).expect("config")).expect("JSONC");
    assert_eq!(
        after["provider"]["cursor-proxy"]["models"]["composer-2.5"]["limit"],
        json!({"context":100000,"output":8192})
    );
    assert!(
        !config::publish_context_limit_for(Version::V1, &path, "composer-2.5", 200000, &cancelled)
            .expect("known limit preserved")
    );
    config::configure_for(Version::V1, &path, None, None, &inventory()).expect("catalog refresh");
    let refreshed = config::parse(&std::fs::read_to_string(&path).expect("config")).expect("JSONC");
    assert_eq!(
        refreshed["provider"]["cursor-proxy"]["models"]["composer-2.5"]["limit"]["context"],
        100000
    );
}

#[test]
fn cross_format_credentials_and_headers_are_not_imported_from_another_endpoint() {
    use cursor_proxy::opencode::Version;
    for version in [Version::V1, Version::V2] {
        for source_endpoint in [Some("https://old.example/v1"), None] {
            let directory = Directory::new();
            let path = directory.0.join("opencode.jsonc");
            let other = version.other();
            let mut source = json!({(other.providers()):{"cursor-proxy":{
                (other.settings()):{"apiKey":"old-provider-secret"},
                "models":{"composer-2.5":{"headers":{"x-api-key":"old-header-secret"}}}
            }},(version.providers()):{"cursor-proxy":{"models":{"composer-2.5":{"headers":{"x-active":"keep"}}}}}});
            if let Some(endpoint) = source_endpoint {
                source[other.providers()]["cursor-proxy"][other.settings()]["baseURL"] =
                    json!(endpoint);
            }
            std::fs::write(&path, source.to_string()).expect("source");
            config::configure_for(
                version,
                &path,
                Some("http://127.0.0.1:32124/v1"),
                None,
                &inventory(),
            )
            .expect("new endpoint");
            let result =
                config::parse(&std::fs::read_to_string(&path).expect("output")).expect("JSONC");
            let target = &result[version.providers()]["cursor-proxy"];
            assert_eq!(target[version.settings()]["apiKey"], "cursor-agent");
            assert_eq!(
                target["models"]["composer-2.5"]["headers"],
                json!({"x-active":"keep"})
            );
            assert_eq!(result[other.providers()], source[other.providers()]);
        }
    }
}

#[test]
fn v1_context_publication_supplies_required_unknown_output_and_keeps_overrides() {
    use cursor_proxy::opencode::Version;
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    config::configure_for(Version::V1, &path, None, None, &inventory()).expect("V1 catalog");
    let cancelled = tokio_util::sync::CancellationToken::new();
    assert!(
        config::publish_context_limit_for(Version::V1, &path, "composer-2.5", 200000, &cancelled)
            .expect("V1 capacity")
    );
    let source = std::fs::read_to_string(&path).expect("configuration");
    let parsed = config::parse(&source).expect("JSONC");
    assert_eq!(
        parsed["provider"]["cursor-proxy"]["models"]["composer-2.5"]["limit"],
        json!({"context":200000,"output":0})
    );
    assert!(parsed.get("providers").is_none());
    assert!(
        !config::publish_context_limit_for(Version::V1, &path, "composer-2.5", 100000, &cancelled)
            .expect("existing limit")
    );
    let updated = config::merge(&source, &json!({"provider":{"cursor-proxy":{"models":{"other-model":{"limit":{"output":8192},"cost":{"input":2,"output":4}}}}}})).expect("explicit output");
    std::fs::write(&path, updated).expect("update fixture");
    assert!(
        config::publish_context_limit_for(Version::V1, &path, "other-model", 100000, &cancelled)
            .expect("missing context")
    );
    let parsed = config::parse(&std::fs::read_to_string(&path).expect("output")).expect("JSONC");
    assert_eq!(
        parsed["provider"]["cursor-proxy"]["models"]["other-model"]["limit"],
        json!({"context":100000,"output":8192})
    );
    assert_eq!(
        parsed["provider"]["cursor-proxy"]["models"]["other-model"]["cost"],
        json!({"input":2,"output":4})
    );
    cancelled.cancel();
    assert!(
        !config::publish_context_limit_for(
            Version::V1,
            &path,
            "not-advertised",
            100000,
            &cancelled
        )
        .expect("cancelled")
    );
}

#[test]
fn switching_formats_inherits_only_known_overrides_and_keeps_source_entries() {
    use cursor_proxy::opencode::Version;
    for version in [Version::V1, Version::V2] {
        let directory = Directory::new();
        let path = directory.0.join("opencode.jsonc");
        let other = version.other();
        let cost = if other == Version::V1 {
            json!({"input":2,"output":8,"cache_read":1,"cache_write":3})
        } else {
            json!({"input":2,"output":8,"cache":{"read":1,"write":3}})
        };
        let mut source = json!({"model":"other/default",(other.providers()):{"cursor-proxy":{
            (other.settings()):{"baseURL":"http://127.0.0.1:32124/v1","apiKey":"inherited-password"},
            "models":{"composer-2.5":{"name":"Old","cost":cost,"limit":{"context":200000,"output":4096},"headers":{"x-test":"keep"},"unmapped":"stay"}}
        }},(version.providers()):{"cursor-proxy":{"models":{"composer-2.5":{"name":"User name","cost":{"input":99}}}},"unrelated":{"name":"Keep"}}});
        source[other.providers()]["cursor-proxy"]["models"]["composer-2.5"][if other == Version::V1
        {
            "id"
        } else {
            "modelID"
        }] = json!("upstream-alias");
        std::fs::write(&path, source.to_string()).expect("source");
        config::configure_for(version, &path, None, None, &inventory()).expect("convert");
        let result =
            config::parse(&std::fs::read_to_string(&path).expect("output")).expect("JSONC");
        assert_eq!(result[other.providers()], source[other.providers()]);
        assert_eq!(
            result[version.providers()]["unrelated"],
            source[version.providers()]["unrelated"]
        );
        let target = &result[version.providers()]["cursor-proxy"];
        assert_eq!(target[version.settings()]["apiKey"], "inherited-password");
        let model = &target["models"]["composer-2.5"];
        assert_eq!(model["name"], "User name");
        assert_eq!(model["cost"]["input"], 99);
        assert_eq!(model["cost"]["output"], 8);
        assert_eq!(model["limit"], json!({"context":200000,"output":4096}));
        assert_eq!(model["headers"]["x-test"], "keep");
        assert_eq!(
            model[if version == Version::V1 {
                "id"
            } else {
                "modelID"
            }],
            "upstream-alias"
        );
        assert!(model.get("unmapped").is_none());
        if version == Version::V1 {
            assert_eq!(model["cost"]["cache_read"], 1);
            assert!(model["cost"].get("cache").is_none());
        } else {
            assert_eq!(model["cost"]["cache"]["write"], 3);
            assert!(model["cost"].get("cache_read").is_none());
        }
        assert!(
            !config::configure_for(version, &path, None, None, &inventory())
                .expect("repeat")
                .changed
        );
    }
}

#[test]
fn v1_catalog_is_native_and_preserves_overrides_across_refreshes() {
    use cursor_proxy::opencode::Version;
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source = "{\n// preserve V1\n\"model\":\"other/current\",\"provider\":{\"other\":{\"name\":\"Keep\"},\"cursor-proxy\":{\"options\":{\"apiKey\":\"saved-password\"},\"models\":{\"composer-2.5\":{\"name\":\"My model\",\"cost\":{\"input\":2,\"output\":8,\"cache_read\":1},\"limit\":{\"context\":99999,\"output\":4096}}}}}}\n";
    std::fs::write(&path, source).expect("source");
    let result = config::configure_for(Version::V1, &path, None, None, &inventory())
        .expect("V1 publication");
    let output = std::fs::read_to_string(&path).expect("updated");
    let parsed = config::parse(&output).expect("JSONC");
    let provider = &parsed["provider"]["cursor-proxy"];
    assert_eq!(provider["npm"], "@ai-sdk/openai-compatible");
    assert_eq!(provider["options"]["baseURL"], "http://127.0.0.1:32124/v1");
    assert_eq!(provider["options"]["apiKey"], "saved-password");
    assert_eq!(provider["models"]["other-model"]["tool_call"], true);
    assert_eq!(
        provider["models"]["other-model"]["interleaved"]["field"],
        "reasoning_content"
    );
    assert_eq!(provider["models"]["composer-2.5"]["name"], "My model");
    assert_eq!(
        provider["models"]["composer-2.5"]["limit"],
        json!({"context":99999,"output":4096})
    );
    assert_eq!(provider["models"]["composer-2.5"]["cost"]["cache_read"], 1);
    assert!(parsed.get("providers").is_none());
    assert!(provider.get("package").is_none() && provider.get("settings").is_none());
    assert!(output.contains("// preserve V1"));
    assert_eq!(parsed["model"], "other/current");
    assert_eq!(
        std::fs::read_to_string(result.backup.expect("backup")).expect("backup"),
        source
    );
    assert!(
        !config::configure_for(Version::V1, &path, None, None, &inventory())
            .expect("idempotent")
            .changed
    );
    let mut expanded = inventory();
    expanded.push(Model {
        id: "new-model".into(),
        name: "New".into(),
    });
    config::configure_for(Version::V1, &path, None, Some("new-password"), &expanded)
        .expect("refresh");
    let parsed = config::parse(&std::fs::read_to_string(&path).expect("updated")).expect("JSONC");
    assert_eq!(
        parsed["provider"]["cursor-proxy"]["options"]["apiKey"],
        "new-password"
    );
    assert!(parsed["provider"]["cursor-proxy"]["models"]["new-model"].is_object());
}

#[test]
fn catalog_and_context_publications_preserve_each_others_changes() {
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    config::configure(&path, None, None, &inventory()).expect("initial catalog");
    let barrier = std::sync::Barrier::new(3);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            barrier.wait();
            let mut catalog = inventory();
            catalog.push(Model {
                id: "new-model".to_owned(),
                name: "New model".to_owned(),
            });
            config::configure(&path, None, None, &catalog).expect("catalog refresh");
        });
        scope.spawn(|| {
            barrier.wait();
            config::publish_context_limit(
                &path,
                "composer-2.5",
                100000,
                &tokio_util::sync::CancellationToken::new(),
            )
            .expect("context");
        });
        barrier.wait();
    });
    let source = std::fs::read_to_string(&path).expect("configuration");
    let parsed = config::parse(&source).expect("JSONC");
    assert_eq!(
        parsed["providers"]["cursor-proxy"]["models"]["composer-2.5"]["limit"]["context"],
        100000
    );
    assert!(parsed["providers"]["cursor-proxy"]["models"]["other-model"].is_object());
    assert_eq!(
        parsed["providers"]["cursor-proxy"]["models"]["new-model"]["name"],
        "New model"
    );
}

#[test]
fn observed_context_capacity_fills_only_missing_limits_and_preserves_prices() {
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source = "{\n// preserve\n\"model\":\"other/current\",\"providers\":{\"cursor-proxy\":{\"models\":{\"composer-2.5\":{\"cost\":{\"input\":2,\"output\":8}},\"custom\":{\"limit\":{\"context\":99999}}}}}}\n";
    std::fs::write(&path, source).expect("source");
    let cancelled = tokio_util::sync::CancellationToken::new();
    assert!(
        config::publish_context_limit(&path, "composer-2.5", 100000, &cancelled)
            .expect("publication")
    );
    let output = std::fs::read_to_string(&path).expect("updated");
    let parsed = config::parse(&output).expect("JSONC");
    assert!(output.contains("// preserve"));
    assert_eq!(
        parsed["providers"]["cursor-proxy"]["models"]["composer-2.5"]["limit"]["context"],
        100000
    );
    assert_eq!(
        parsed["providers"]["cursor-proxy"]["models"]["composer-2.5"]["cost"],
        json!({"input":2,"output":8})
    );
    assert_eq!(parsed["model"], "other/current");
    assert!(
        !config::publish_context_limit(&path, "composer-2.5", 200000, &cancelled)
            .expect("existing observed limit")
    );
    assert!(
        !config::publish_context_limit(&path, "custom", 200000, &cancelled).expect("custom limit")
    );
    assert!(
        !config::publish_context_limit(&path, "not-advertised", 100000, &cancelled)
            .expect("unknown model")
    );
    assert_eq!(output, std::fs::read_to_string(&path).expect("unchanged"));
    let missing = directory.0.join("missing.json");
    assert!(
        !config::publish_context_limit(&missing, "composer-2.5", 100000, &cancelled)
            .expect("missing config")
    );
    assert!(!missing.exists());
    std::fs::write(&path, source).expect("reset own fixture");
    cancelled.cancel();
    assert!(
        !config::publish_context_limit(&path, "composer-2.5", 100000, &cancelled)
            .expect("cancelled")
    );
    assert_eq!(std::fs::read_to_string(&path).expect("original"), source);
}

#[test]
fn jsonc_merge_preserves_comments_strings_and_unrelated_settings() {
    let source = r#"{
  // Keep the selected model.
  "model": "other/current",
  "url": "https://example.test/a//b/*c*/",
  "quoted": "\"} // not a comment",
  "providers": {
    "other": {"models": {"current": {}}},
    "lmstudio": {
      /* Keep custom request headers. */
      "headers": {"x-project": "keep"},
      "settings": {"apiKey": "existing-token",},
    },
  },
  "plugins": ["existing-plugin",],
}
"#;
    let original = config::parse(source).expect("source JSONC");
    let patch = models::provider_config(
        &inventory(),
        "http://127.0.0.1:32124/v1",
        Some("existing-token"),
        "cursor-proxy",
    );
    let output = config::merge(source, &patch).expect("merged JSONC");
    let value = config::parse(&output).expect("output JSONC");
    assert!(output.contains("// Keep the selected model."));
    assert!(output.contains("/* Keep custom request headers. */"));
    for key in ["model", "url", "quoted", "plugins"] {
        assert_eq!(value[key], original[key]);
    }
    assert_eq!(value["providers"]["other"], original["providers"]["other"]);
    assert_eq!(
        value["providers"]["lmstudio"]["headers"]["x-project"],
        "keep"
    );
    assert_eq!(
        value["providers"]["lmstudio"]["settings"]["apiKey"],
        "existing-token"
    );
    assert_eq!(
        value["providers"]["lmstudio"],
        original["providers"]["lmstudio"]
    );
    assert_eq!(
        value["providers"]["cursor-proxy"]["models"]["other-model"]["capabilities"]["tools"],
        true
    );
    assert_eq!(
        config::merge(&output, &patch).expect("repeat merge"),
        output
    );
}

#[test]
fn jsonc_merge_handles_empty_objects_bom_and_comment_placement() {
    let patch = json!({"providers": {"cursor-proxy": {"name": "Cursor Proxy"}}});
    for source in [
        "{}",
        "{ /* keep */ }",
        "\u{feff}{}",
        "{\"a\":1 // keep\n}",
        "{\"a\":1, /* keep */}",
    ] {
        let output = config::merge(source, &patch).expect("merge");
        let value = config::parse(&output).expect("parse merged config");
        assert_eq!(value["providers"]["cursor-proxy"]["name"], "Cursor Proxy");
        if source.contains("keep") {
            assert!(output.contains("keep"));
        }
    }
}

#[test]
fn invalid_or_ambiguous_jsonc_is_not_rewritten() {
    let patch = json!({"providers": {"cursor-proxy": {"name": "Cursor Proxy"}}});
    for source in [
        "{,}",
        "{\"a\":[,]}",
        "{\"a\":,}",
        "{/* missing",
        "{\"providers\":null}",
        "{\"providers\":{},\"providers\":{}}",
        "[]",
    ] {
        assert!(config::merge(source, &patch).is_err(), "{source}");
    }
}

#[test]
fn configure_preserves_tokens_backs_up_original_and_is_idempotent() {
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source = "{\n// keep\n\"model\":\"other/current\",\"providers\":{\"cursor-proxy\":{\"settings\":{\"apiKey\":\"keep-token\"},\"models\":{\"composer-2.5\":{\"name\":\"My Composer\",\"capabilities\":{\"tools\":false},\"limit\":{\"context\":12345}}}}}\n}\n";
    std::fs::write(&path, source).expect("existing config");
    let result = config::configure(&path, None, None, &inventory()).expect("configure");
    assert!(result.changed);
    assert_eq!(
        std::fs::read(result.backup.expect("backup path")).expect("backup"),
        source.as_bytes()
    );
    let merged = std::fs::read_to_string(&path).expect("merged file");
    let value = config::parse(&merged).expect("merged config");
    assert_eq!(value["model"], "other/current");
    assert_eq!(
        value["providers"]["cursor-proxy"]["settings"]["apiKey"],
        "keep-token"
    );
    assert_eq!(
        value["providers"]["cursor-proxy"]["models"]["composer-2.5"]["name"],
        "My Composer"
    );
    assert_eq!(
        value["providers"]["cursor-proxy"]["models"]["composer-2.5"]["capabilities"]["tools"],
        false
    );
    assert_eq!(
        value["providers"]["cursor-proxy"]["models"]["composer-2.5"]["limit"]["context"],
        12345
    );
    let count = std::fs::read_dir(&directory.0).expect("directory").count();
    let repeated = config::configure(&path, None, None, &inventory()).expect("configure again");
    assert!(!repeated.changed);
    assert!(repeated.backup.is_none());
    assert_eq!(
        std::fs::read_to_string(&path).expect("unchanged file"),
        merged
    );
    assert_eq!(
        std::fs::read_dir(&directory.0).expect("directory").count(),
        count
    );
}

#[test]
fn configure_preserves_real_lmstudio_and_refuses_invalid_input_without_writing() {
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source =
        "{\"providers\":{\"lmstudio\":{\"settings\":{\"baseURL\":\"http://127.0.0.1:1234/v1\"}}}}";
    std::fs::write(&path, source).expect("existing config");
    assert!(config::configure(&path, None, None, &[]).is_err());
    assert!(config::configure(&path, Some("http://localhost:32124"), None, &inventory()).is_err());
    assert!(
        config::configure(
            &path,
            Some("http://localhost:32124/v1"),
            Some("bad token"),
            &inventory()
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(&path).expect("unchanged config"),
        source
    );
    assert_eq!(
        std::fs::read_dir(&directory.0).expect("directory").count(),
        1
    );
    let result =
        config::configure(&path, None, None, &inventory()).expect("independent Cursor provider");
    assert!(result.changed);
    let value = config::parse(&std::fs::read_to_string(&path).expect("config")).expect("JSONC");
    assert_eq!(
        value["providers"]["lmstudio"],
        config::parse(source).expect("original")["providers"]["lmstudio"]
    );
    assert_eq!(
        value["providers"]["cursor-proxy"]["settings"]["baseURL"],
        "http://127.0.0.1:32124/v1"
    );
}

#[tokio::test]
async fn cli_only_exposes_automatic_server_options() {
    use std::time::Duration;
    use tokio::process::Command;
    let directory = Directory::new();
    for arguments in [vec!["-h"], vec!["--help"]] {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
                .args(&arguments)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("CLI help deadline")
        .expect("CLI help");
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).expect("help text");
        for removed in [
            "configure-opencode",
            "print-config",
            "--config",
            "--opencode-config",
            "--base-url",
            "--provider",
            "--session-resume",
            "--token",
            "Commands:",
        ] {
            assert!(!help.contains(removed), "{help}");
        }
        for option in [
            "--host",
            "--port",
            "--workspace",
            "--cursor-agent",
            "--password",
            "--timeout-secs",
            "--load-skills",
        ] {
            assert_eq!(
                help.split_whitespace()
                    .filter(|word| *word == option)
                    .count(),
                1,
                "{help}"
            );
        }
    }
    for arguments in [
        vec!["configure-opencode"],
        vec!["print-config"],
        vec!["--config", "unused.json"],
        vec!["--opencode-config", "unused.json"],
        vec!["--session-resume"],
        vec!["--token", "obsolete-password"],
        vec!["serve"],
        vec!["help", "serve"],
    ] {
        let output = tokio::time::timeout(
            Duration::from_secs(5),
            Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
                .args(arguments)
                .arg("--cursor-agent")
                .arg(directory.0.join("missing-agent"))
                .current_dir(&directory.0)
                .env("XDG_CONFIG_HOME", &directory.0)
                .env_remove("OPENCODE_CONFIG_DIR")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .expect("removed entry deadline")
        .expect("removed CLI entry");
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("unexpected argument") || error.contains("unrecognized subcommand"),
            "{error}"
        );
    }
}

#[test]
fn configure_does_not_depend_on_the_lmstudio_plugin() {
    let directory = Directory::new();
    let path = directory.0.join("opencode.jsonc");
    let source = r#"{"plugins":["-opencode.provider.lmstudio"]}"#;
    std::fs::write(&path, source).expect("disabled discovery config");
    config::configure(&path, None, None, &inventory()).expect("independent provider");
    let value = config::parse(&std::fs::read_to_string(&path).expect("config")).expect("JSONC");
    assert_eq!(value["plugins"], json!(["-opencode.provider.lmstudio"]));
    assert!(value["providers"].get("lmstudio").is_none());
}

#[test]
fn configure_migrates_generated_lmstudio_alias_without_duplicate_cursor_providers() {
    for (providers, trailing) in [
        (
            r#""lmstudio":{"name":"Cursor Proxy","package":"@opencode/ai/providers/openai-compatible","compatibility":{"reasoningField":"reasoning_content"},"settings":{"baseURL":"http://127.0.0.1:32124/v1","apiKey":"legacy-token"}},"cursor-proxy":{"models":{"custom":{"name":"Keep"}}}"#,
            "",
        ),
        (
            r#""cursor-proxy":{"models":{"custom":{"name":"Keep"}}},"lmstudio":{"name":"Cursor Proxy","package":"@opencode/ai/providers/openai-compatible","compatibility":{"reasoningField":"reasoning_content"},"settings":{"baseURL":"http://127.0.0.1:32124/v1","apiKey":"legacy-token"}}"#,
            "",
        ),
        (
            r#""lmstudio":{"name":"Cursor Proxy","package":"@opencode/ai/providers/openai-compatible","compatibility":{"reasoningField":"reasoning_content"},"settings":{"baseURL":"http://127.0.0.1:32124/v1","apiKey":"legacy-token"}}"#,
            ",",
        ),
    ] {
        let directory = Directory::new();
        let path = directory.0.join("opencode.jsonc");
        let source = format!(
            "{{\n// keep user settings\n\"model\":\"lmstudio/composer-2.5\",\"agents\":{{\"plan\":{{\"model\":{{\"providerID\":\"lmstudio\",\"model\":\"other-model\"}}}}}},\"providers\":{{{providers}{trailing}}}}}"
        );
        std::fs::write(&path, &source).expect("legacy configuration");
        let result =
            config::configure(&path, None, None, &inventory()).expect("migrate legacy provider");
        let output = std::fs::read_to_string(&path).expect("migrated file");
        let value = config::parse(&output).expect("migrated JSONC");
        assert!(output.contains("// keep user settings"));
        assert!(value["providers"].get("lmstudio").is_none());
        assert_eq!(value["providers"].as_object().expect("providers").len(), 1);
        assert_eq!(
            value["providers"]["cursor-proxy"]["settings"]["apiKey"],
            "legacy-token"
        );
        assert_eq!(
            value["providers"]["cursor-proxy"]["models"]["other-model"]["capabilities"]["tools"],
            true
        );
        assert_eq!(value["model"], "cursor-proxy/composer-2.5");
        assert_eq!(
            value["agents"]["plan"]["model"]["providerID"],
            "cursor-proxy"
        );
        assert_eq!(
            std::fs::read_to_string(result.backup.expect("backup")).expect("original backup"),
            source
        );
        assert!(
            !config::configure(&path, None, None, &inventory())
                .expect("idempotent migration")
                .changed
        );
    }
}
