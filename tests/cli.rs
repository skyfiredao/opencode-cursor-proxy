use std::process::Command;

#[test]
fn removed_session_resume_is_not_advertised_or_accepted() {
    for args in [["-h"], ["--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
            .args(args)
            .output()
            .expect("run CLI help");
        assert!(output.status.success());
        let help = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(!help.contains("--session-resume"), "{help}");
    }
    let output = Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
        .args(["--session-resume", "--timeout-secs", "0"])
        .output()
        .expect("run removed option");
    assert_eq!(output.status.code(), Some(2));
    let error = String::from_utf8(output.stderr).expect("UTF-8 error");
    assert!(
        error.contains("unexpected argument '--session-resume'"),
        "{error}"
    );
}

#[test]
fn password_checks_use_the_public_option_name_before_startup() {
    for (args, expected) in [
        (vec!["--password", ""], "--password must be nonempty"),
        (
            vec!["--password", "with spaces"],
            "--password must be nonempty",
        ),
        (vec!["--host", "0.0.0.0"], "Set --password"),
        (
            vec![
                "--host",
                "0.0.0.0",
                "--password",
                "fixture-password",
                "--timeout-secs",
                "0",
            ],
            "--timeout-secs must be greater than zero",
        ),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
            .args(args)
            .output()
            .expect("run password validation");
        assert_eq!(output.status.code(), Some(1));
        let error = String::from_utf8(output.stderr).expect("UTF-8 error");
        assert!(error.contains(expected), "{error}");
        assert!(!error.contains("--token"), "{error}");
        assert!(!error.contains("fixture-password"), "{error}");
    }
}

#[test]
fn root_help_lists_only_direct_options_and_their_purpose() {
    let mut previous_help = None;
    for args in [vec!["-h"], vec!["--help"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_cursor-proxy"))
            .args(&args)
            .output()
            .expect("run CLI");
        assert!(output.status.success(), "{args:?}: {output:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
        let help = String::from_utf8(output.stdout).expect("UTF-8 help");
        assert!(help.contains("Usage: cursor-proxy [OPTIONS]"), "{help}");
        let options = help.split_once("Options:\n").expect("options heading").1;
        for option in [
            "--host <HOST>",
            "--port <PORT>",
            "--password <PASSWORD>",
            "--workspace <WORKSPACE>",
            "--cursor-agent <CURSOR_AGENT>",
            "--timeout-secs <TIMEOUT_SECS>",
            "--load-skills",
            "-h, --help",
            "-V, --version",
        ] {
            assert!(
                options.lines().any(|line| line == format!("  {option}")),
                "{args:?}: {help}"
            );
        }
        for purpose in [
            "Listen address",
            "Listen port",
            "Set an access password",
            "Project directory",
            "Cursor CLI path",
            "Reply timeout in seconds",
            "Load project instructions",
        ] {
            assert!(options.contains(&format!("\n    {purpose}")), "{help}");
        }
        assert!(options.contains("[default: 127.0.0.1]"), "{help}");
        for removed in [
            "Commands:",
            "serve",
            "--token",
            "Bearer",
            "Authorization",
            "x-opencode-directory",
            "auto-trusted",
            "AGENTS.md",
        ] {
            assert!(!help.contains(removed), "{removed}: {help}");
        }
        assert!(
            !options.lines().any(|line| line.starts_with("     ")),
            "{help}"
        );
        if let Some(previous) = &previous_help {
            assert_eq!(previous, &help, "short and long help must be identical");
        }
        previous_help = Some(help);
    }
}
