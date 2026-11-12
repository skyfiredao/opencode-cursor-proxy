use std::{net::IpAddr, path::PathBuf, sync::Arc, time::Duration};

use clap::{CommandFactory, FromArgMatches, Parser};
use cursor_proxy::{agent::AgentConfig, config, models::ModelCache, server};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "cursor-proxy", version, about = "Connect OpenCode to Cursor")]
struct Cli {
    #[arg(long, default_value = "127.0.0.1", help = "Listen address")]
    host: IpAddr,
    #[arg(long, default_value_t = 32124, help = "Listen port")]
    port: u16,
    #[arg(long, help = "Set an access password")]
    password: Option<String>,
    #[arg(long, help = "Project directory")]
    workspace: Option<PathBuf>,
    #[arg(long, default_value = "cursor-agent", help = "Cursor CLI path")]
    cursor_agent: PathBuf,
    #[arg(long, default_value_t = 300, help = "Reply timeout in seconds")]
    timeout_secs: u64,
    #[arg(long, help = "Load project instructions")]
    load_skills: bool,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let mut command = Cli::command();
    command.build();
    let help = command
        .clone()
        .next_line_help(true)
        .render_help()
        .to_string()
        .lines()
        .map(|line| {
            if line.starts_with("          ") {
                &line[6..]
            } else {
                line.strip_prefix("    ").unwrap_or(line)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let command = command.override_help(format!("{help}\n"));
    serve(Cli::from_arg_matches(&command.get_matches())?).await
}

async fn serve(args: Cli) -> Result<(), Box<dyn std::error::Error>> {
    if args
        .password
        .as_ref()
        .is_some_and(|s| s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()))
    {
        return Err(
            "--password must be nonempty and contain no whitespace or control characters".into(),
        );
    }
    if !args.host.is_loopback() && args.password.is_none() {
        return Err("Set --password when listening outside this computer".into());
    }
    if args.timeout_secs == 0 {
        return Err("--timeout-secs must be greater than zero".into());
    }
    let config = config::default_path()?;
    let workspace = match args.workspace {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let workspace = tokio::fs::canonicalize(workspace).await?;
    if !tokio::fs::metadata(&workspace).await?.is_dir() {
        return Err("--workspace must be a directory".into());
    }
    let listener = tokio::net::TcpListener::bind((args.host, args.port)).await?;
    let address = listener.local_addr()?;
    let host = match address.ip() {
        std::net::IpAddr::V4(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        }
        std::net::IpAddr::V6(ip) if ip.is_unspecified() => {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        }
        host => host,
    };
    let base_url = format!(
        "http://{}/v1",
        std::net::SocketAddr::new(host, address.port())
    );
    tracing::info!(
        addr = %listener.local_addr()?,
        workspace = %workspace.display(),
        cursor_agent = %args.cursor_agent.display(),
        password = args.password.is_some(),
        "listening"
    );

    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
    let version = match cursor_proxy::opencode::detect(std::path::Path::new("opencode")).await {
        Ok(version) => version,
        Err(error) => {
            tracing::warn!(%error, "Cannot detect OpenCode version; using V2 configuration");
            cursor_proxy::opencode::Version::V2
        }
    };
    tracing::info!(?version, "selected OpenCode configuration format");
    let trusted_workspace = Some(workspace.clone());
    let state = Arc::new(server::AppState {
        workspace,
        agent: AgentConfig {
            binary: resolve_binary(args.cursor_agent)?,
            timeout: Duration::from_secs(args.timeout_secs),
            trusted_workspace,
            load_skills: args.load_skills,
        },
        token: args.password,
        models: ModelCache::new(version),
        sessions: Arc::new(cursor_proxy::session::SessionStore::with_configuration(
            config.clone(),
            version,
        )),
        shutdown: shutdown_rx.clone(),
    });
    let model_state = state.clone();
    let refresher = tokio::spawn(async move {
        model_state
            .models
            .maintain(
                &model_state.agent.binary,
                &config,
                &base_url,
                model_state.token.as_deref(),
                shutdown_rx,
            )
            .await;
    });
    let signal_tx = shutdown_tx.clone();
    let result = axum::serve(listener, server::router(state))
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            if let Err(error) = signal_tx.send(true) {
                tracing::debug!(%error, "shutdown receivers already closed");
            }
        })
        .await;
    let _stopped = shutdown_tx.send(true);
    refresher.await?;
    result?;
    Ok(())
}

fn resolve_binary(path: PathBuf) -> Result<PathBuf, std::io::Error> {
    if path.components().count() > 1 {
        #[cfg(windows)]
        {
            std::path::absolute(path)
        }
        #[cfg(not(windows))]
        {
            std::fs::canonicalize(path)
        }
    } else {
        Ok(path)
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result { tracing::error!(%error, "cannot listen for Ctrl-C"); }
                    }
                    _ = term.recv() => (),
                }
            }
            Err(error) => {
                tracing::error!(%error, "cannot listen for termination signal");
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        tracing::error!(%error, "cannot listen for Ctrl-C");
    }
}
