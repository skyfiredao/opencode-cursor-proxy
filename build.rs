use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout)
        .ok()
        .map(|value| value.trim().to_owned())
}

fn package_version(manifest: &str) -> Option<&str> {
    let mut package = false;
    for line in manifest.lines().map(str::trim) {
        if line.starts_with('[') {
            package = line.split('#').next()?.trim() == "[package]";
        } else if package
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "version"
        {
            let value = value.trim();
            let quote = value.chars().next()?;
            if matches!(quote, '\'' | '"') {
                return value.get(1..)?.split(quote).next();
            }
            return None;
        }
    }
    None
}

fn patch_count(commit: &str) -> Option<u64> {
    if git(&["rev-parse", "--is-shallow-repository"])? != "false" {
        return None;
    }
    let manifest = format!("{}Cargo.toml", git(&["rev-parse", "--show-prefix"])?);
    let series = format!(
        "{}.{}.",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR")
    );
    let revisions = git(&[
        "log",
        "--first-parent",
        "--format=%H",
        commit,
        "--",
        "Cargo.toml",
    ])?;
    let mut baseline = None;
    for revision in revisions.lines() {
        let contents = git(&["show", &format!("{revision}:{manifest}")])?;
        if !package_version(&contents)?.starts_with(&series) {
            break;
        }
        baseline = Some(revision);
    }
    git(&[
        "rev-list",
        "--count",
        &format!("{}..{commit}", baseline?),
        "--",
        ".",
    ])?
    .parse()
    .ok()
}

fn main() {
    println!("cargo:rerun-if-env-changed=CURSOR_PROXY_BUILD_COMMIT");
    for reference in [
        Some("HEAD".to_owned()),
        git(&["symbolic-ref", "-q", "HEAD"]),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(path) = git(&["rev-parse", "--git-path", &reference]) {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    for reference in ["packed-refs", "shallow"] {
        if let Some(path) = git(&["rev-parse", "--git-path", reference])
            && std::path::Path::new(&path).is_file()
        {
            println!("cargo:rerun-if-changed={path}");
        }
    }
    let commit = std::env::var("CURSOR_PROXY_BUILD_COMMIT")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| git(&["rev-parse", "--verify", "HEAD"]))
        .filter(|value| value.len() >= 7 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .unwrap_or_else(|| {
            println!("cargo:warning=Git SHA unavailable; set CURSOR_PROXY_BUILD_COMMIT for build identification");
            "unknown".to_owned()
        });
    println!("cargo:rustc-env=CURSOR_PROXY_BUILD_COMMIT={commit}");
    let patch = patch_count(&commit).map(|value| value.to_string()).unwrap_or_else(|| {
        if commit != "unknown" {
            println!("cargo:warning=Complete project history unavailable; using the manifest patch version");
        }
        env!("CARGO_PKG_VERSION_PATCH").to_owned()
    });
    println!(
        "cargo:rustc-env=CURSOR_PROXY_BUILD_VERSION={}.{}.{}+{}",
        env!("CARGO_PKG_VERSION_MAJOR"),
        env!("CARGO_PKG_VERSION_MINOR"),
        patch,
        commit.get(..7).unwrap_or("unknown")
    );
}
