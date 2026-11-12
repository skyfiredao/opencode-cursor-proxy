use crate::stream::Usage;
use rusqlite::{Connection, OpenFlags, OptionalExtension, limits::Limit};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::ErrorKind,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub root: String,
    pub used: u64,
    pub capacity: u64,
}

#[derive(Debug, Clone)]
pub enum Baseline {
    Empty,
    Root(String),
    Unavailable,
}

pub async fn baseline(session: &str) -> Baseline {
    match observe(session).await {
        Ok(Some(checkpoint)) => Baseline::Root(checkpoint.root),
        Ok(None) => Baseline::Empty,
        Err(_) => Baseline::Unavailable,
    }
}

async fn observe(session: &str) -> Result<Option<Checkpoint>, String> {
    let config = cursor_config().ok_or("Cursor context directory unavailable")?;
    let session = session.to_owned();
    let task = tokio::task::spawn_blocking(move || read_checkpoint(&config, &session));
    match tokio::time::timeout(Duration::from_millis(50), task).await {
        Ok(Ok(Ok(snapshot))) => Ok(snapshot),
        Ok(Ok(Err(error))) => {
            tracing::debug!(%error, "ACP context checkpoint unavailable");
            Err(error)
        }
        Ok(Err(error)) => {
            tracing::debug!(%error, "ACP context observation failed");
            Err(error.to_string())
        }
        Err(_) => {
            tracing::debug!("ACP context observation budget expired");
            Err("ACP context observation budget expired".to_owned())
        }
    }
}

pub async fn publish_capacity(
    version: crate::opencode::Version,
    path: PathBuf,
    model: String,
    capacity: u64,
) {
    let cancelled = tokio_util::sync::CancellationToken::new();
    let _guard = cancelled.clone().drop_guard();
    let task = tokio::task::spawn_blocking(move || {
        crate::config::publish_context_limit_for(version, &path, &model, capacity, &cancelled)
            .map_err(|error| error.to_string())
    });
    match tokio::time::timeout(Duration::from_millis(100), task).await {
        Ok(Ok(Ok(_))) => (),
        Ok(Ok(Err(error))) => {
            tracing::debug!(%error, "OpenCode context capacity publication unavailable")
        }
        Ok(Err(error)) => tracing::debug!(%error, "OpenCode context capacity worker failed"),
        Err(_) => tracing::debug!("OpenCode context capacity publication budget expired"),
    }
}

pub async fn resolve(
    reported: Option<Usage>,
    context: Option<Usage>,
    session: &str,
    before: &Baseline,
) -> Option<Usage> {
    if let Some(mut usage) = reported {
        usage.context_window = context.as_ref().and_then(|usage| usage.context_window);
        return Some(usage);
    }
    if context.is_some() {
        return context;
    }
    if matches!(before, Baseline::Unavailable) {
        return None;
    }
    let Ok(Some(checkpoint)) = observe(session).await else {
        return None;
    };
    if matches!(before, Baseline::Root(root) if *root == checkpoint.root) {
        return None;
    }
    Usage::from_context(checkpoint.used, checkpoint.capacity)
}

pub fn cursor_config() -> Option<PathBuf> {
    let variable =
        |key| std::env::var_os(key).filter(|value| !value.to_string_lossy().trim().is_empty());
    if let Some(path) = variable("CURSOR_CONFIG_DIR") {
        return Some(PathBuf::from(path));
    }
    if let Some(path) = variable("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(path).join("cursor"));
    }
    #[cfg(windows)]
    let home = variable("USERPROFILE").or_else(|| variable("HOME"));
    #[cfg(not(windows))]
    let home = variable("HOME").or_else(|| variable("USERPROFILE"));
    home.map(|path| PathBuf::from(path).join(".cursor"))
}

pub fn read_checkpoint(config: &Path, session: &str) -> Result<Option<Checkpoint>, String> {
    let read = || -> Result<Option<Checkpoint>, Box<dyn std::error::Error>> {
        if Uuid::parse_str(session)?.to_string() != session {
            return Err("Noncanonical ACP session ID".into());
        }
        let base = match config.join("acp-sessions").canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let expected = base.join(session).join("store.db");
        let path = match expected.canonicalize() {
            Ok(path) => path,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if path != expected || !path.is_file() {
            return Err("ACP checkpoint path is not a regular session store".into());
        }
        let db = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        db.busy_timeout(Duration::from_millis(25))?;
        db.set_limit(Limit::SQLITE_LIMIT_LENGTH, 2 * 1024 * 1024 + 1024)?;
        db.execute_batch("PRAGMA query_only=ON; BEGIN;")?;
        let version: u32 = db.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version != 1 {
            return Err("Unsupported ACP checkpoint schema".into());
        }
        let metadata: Option<String> = db
            .query_row(
                "SELECT value FROM meta WHERE key='0' AND length(value)<=65536",
                [],
                |row| row.get(0),
            )
            .optional()?;
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        let metadata = decode_hex(&metadata)?;
        #[derive(Deserialize)]
        struct Metadata {
            #[serde(rename = "latestRootBlobId")]
            root: String,
        }
        let metadata: Metadata = serde_json::from_slice(&metadata)?;
        if metadata.root.is_empty() {
            return Ok(None);
        }
        if metadata.root.len() != 64 || !metadata.root.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err("Invalid ACP checkpoint root".into());
        }
        let root = metadata.root.to_ascii_lowercase();
        let data: Option<Vec<u8>> = db
            .query_row(
                "SELECT data FROM blobs WHERE id=?1 AND length(data)<=2097152",
                [&root],
                |row| row.get(0),
            )
            .optional()?;
        let Some(data) = data else {
            return Ok(None);
        };
        if format!("{:x}", Sha256::digest(&data)) != root {
            return Err("ACP checkpoint hash mismatch".into());
        }
        let mut remaining = data.as_slice();
        let mut details = None;
        while let Some((number, value)) = field(&mut remaining)? {
            if number == 5 {
                if details.is_some() {
                    return Err("Duplicate ACP context record".into());
                }
                let Field::Bytes(value) = value else {
                    return Err("Invalid ACP context record".into());
                };
                details = Some(value);
            }
        }
        let Some(mut details) = details else {
            return Ok(None);
        };
        let mut used = None;
        let mut capacity = None;
        while let Some((number, value)) = field(&mut details)? {
            if matches!(number, 1 | 2) {
                let Field::Integer(value) = value else {
                    return Err("Invalid ACP context counter".into());
                };
                let value = u32::try_from(value)? as u64;
                let target = if number == 1 {
                    &mut used
                } else {
                    &mut capacity
                };
                if target.replace(value).is_some() {
                    return Err("Duplicate ACP context counter".into());
                }
            }
        }
        let Some(capacity) = capacity.filter(|value| *value > 0) else {
            return Ok(None);
        };
        Ok(Some(Checkpoint {
            root,
            used: used.unwrap_or(0),
            capacity,
        }))
    };
    read().map_err(|error| error.to_string())
}

fn decode_hex(value: &str) -> Result<Vec<u8>, String> {
    let (pairs, remainder) = value.as_bytes().as_chunks::<2>();
    if !remainder.is_empty() {
        return Err("Invalid ACP metadata encoding".into());
    }
    pairs
        .iter()
        .map(|pair| {
            let digit = |byte: u8| {
                (byte as char)
                    .to_digit(16)
                    .ok_or("Invalid ACP metadata encoding".to_owned())
            };
            Ok((digit(pair[0])? * 16 + digit(pair[1])?) as u8)
        })
        .collect()
}

enum Field<'a> {
    Integer(u64),
    Bytes(&'a [u8]),
    Skipped,
}

fn integer(input: &mut &[u8]) -> Result<u64, String> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let (&byte, rest) = input
            .split_first()
            .ok_or("Truncated ACP checkpoint integer")?;
        *input = rest;
        if shift == 63 && byte > 1 {
            return Err("Overflowing ACP checkpoint integer".into());
        }
        value |= u64::from(byte & 127) << shift;
        if byte < 128 {
            return Ok(value);
        }
    }
    Err("Overflowing ACP checkpoint integer".into())
}

fn field<'a>(input: &mut &'a [u8]) -> Result<Option<(u32, Field<'a>)>, String> {
    if input.is_empty() {
        return Ok(None);
    }
    let tag = integer(input)?;
    let number = u32::try_from(tag >> 3).map_err(|_| "Invalid ACP checkpoint field")?;
    if number == 0 || number > 536_870_911 {
        return Err("Invalid ACP checkpoint field".into());
    }
    let wire = tag & 7;
    if wire == 0 {
        return Ok(Some((number, Field::Integer(integer(input)?))));
    }
    let length = match wire {
        1 => 8,
        2 => usize::try_from(integer(input)?).map_err(|_| "Oversized ACP checkpoint field")?,
        5 => 4,
        _ => return Err("Unsupported ACP checkpoint wire type".into()),
    };
    let value = input
        .get(..length)
        .ok_or("Truncated ACP checkpoint field")?;
    *input = &input[length..];
    Ok(Some((
        number,
        if wire == 2 {
            Field::Bytes(value)
        } else {
            Field::Skipped
        },
    )))
}
