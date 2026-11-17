use crate::error::ApiError;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

pub(super) const RETENTION: i64 = 90 * 24 * 60 * 60;

pub(super) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .min(i64::MAX as u64) as i64
}

#[derive(Clone)]
pub(super) struct Mapping {
    pub id: String,
    pub owner: Option<String>,
    pub workspace: PathBuf,
    pub model: String,
    pub cursor_id: String,
    pub history_hash: String,
    pub history_len: usize,
    pub instructions_hash: String,
    pub ready: bool,
    pub created_at: i64,
    pub last_used_at: i64,
}

#[derive(Clone)]
pub(super) struct Database {
    path: PathBuf,
}

fn failure(error: impl std::fmt::Display) -> ApiError {
    ApiError::upstream(format!("Proxy session database: {error}"))
}

impl Database {
    pub async fn open(path: PathBuf) -> Result<Self, ApiError> {
        let database = Self { path };
        database
            .run(|connection| {
                let version: u32 =
                    connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                if version > 1 {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                connection.execute_batch(
                    "PRAGMA journal_mode=WAL;
                CREATE TABLE IF NOT EXISTS sessions (
                    id TEXT PRIMARY KEY, owner TEXT NOT NULL, workspace TEXT NOT NULL,
                    model TEXT NOT NULL, cursor_id TEXT NOT NULL,
                    history_hash TEXT NOT NULL, history_len INTEGER NOT NULL,
                    instructions_hash TEXT NOT NULL, ready INTEGER NOT NULL,
                    created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL
                );
                CREATE INDEX IF NOT EXISTS sessions_owner ON sessions(owner,workspace,model);
                 CREATE INDEX IF NOT EXISTS sessions_age ON sessions(last_used_at);
                 CREATE INDEX IF NOT EXISTS sessions_history ON sessions(owner,workspace,model,history_len DESC,history_hash,last_used_at DESC);
                PRAGMA user_version=1;",
                )
            })
            .await?;
        Ok(database)
    }

    async fn run<T: Send + 'static>(
        &self,
        action: impl FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(failure)?;
            }
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(_) => (),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => (),
                Err(error) => return Err(failure(error)),
            }
            let mut connection = Connection::open(path).map_err(failure)?;
            connection
                .busy_timeout(Duration::from_secs(2))
                .map_err(failure)?;
            action(&mut connection).map_err(failure)
        })
        .await
        .map_err(failure)?
    }

    pub async fn candidates(
        &self,
        owner: &Option<String>,
        workspace: &std::path::Path,
        model: &str,
        boundaries: Vec<String>,
    ) -> Result<Vec<Mapping>, ApiError> {
        let Some(owner) = owner.clone() else {
            return Ok(Vec::new());
        };
        let workspace = workspace.to_owned();
        let model = model.to_owned();
        self.run(move |connection| {
            let mut query = connection.prepare("SELECT sessions.id,cursor_id,history_hash,history_len,instructions_hash,ready,created_at,last_used_at
                FROM sessions JOIN json_each(?4) history ON history_len=CAST(history.key AS INTEGER)+1 AND history_hash=history.value
                WHERE owner=?1 AND workspace=?2 AND model=?3 AND last_used_at>=?5
                AND (ready=0 OR history_len<json_array_length(?4))
                ORDER BY history_len DESC,last_used_at DESC LIMIT ?6")?;
            query.query_map(params![owner,workspace.to_string_lossy(),model,serde_json::json!(boundaries).to_string(),now()-RETENTION,super::MAX_LIVE as i64 + 1], |row| Ok(Mapping {
                id: row.get(0)?, owner: Some(owner.clone()), workspace: workspace.clone(), model: model.clone(),
                cursor_id: row.get(1)?, history_hash: row.get(2)?, history_len: usize::try_from(row.get::<_,i64>(3)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
                instructions_hash: row.get(4)?, ready: row.get(5)?, created_at: row.get(6)?, last_used_at: row.get(7)?,
            }))?.collect()
        }).await
    }

    pub async fn save(&self, mapping: &Mapping) -> Result<(), ApiError> {
        if mapping.owner.is_none() {
            return Ok(());
        }
        let mapping = mapping.clone();
        self.run(move |connection| {
            connection.execute("INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                ON CONFLICT(id) DO UPDATE SET cursor_id=excluded.cursor_id,history_hash=excluded.history_hash,
                history_len=excluded.history_len,instructions_hash=excluded.instructions_hash,ready=excluded.ready,
                created_at=excluded.created_at,last_used_at=excluded.last_used_at",
                params![mapping.id,mapping.owner,mapping.workspace.to_string_lossy(),mapping.model,mapping.cursor_id,
                    mapping.history_hash,i64::try_from(mapping.history_len).map_err(|_| rusqlite::Error::InvalidQuery)?,mapping.instructions_hash,mapping.ready,mapping.created_at,mapping.last_used_at])?;
            Ok(())
        }).await
    }

    pub async fn refresh(&self, mapping: &Mapping) -> Result<Option<Mapping>, ApiError> {
        let mapping = mapping.clone();
        self.run(move |connection| {
            connection.query_row(
                "SELECT cursor_id,history_hash,history_len,instructions_hash,ready,created_at,last_used_at
                 FROM sessions WHERE id=?1 AND owner=?2 AND workspace=?3 AND model=?4 AND last_used_at>=?5",
                params![mapping.id,mapping.owner,mapping.workspace.to_string_lossy(),mapping.model,now()-RETENTION],
                |row| {
                    let mut current = mapping.clone();
                    current.cursor_id = row.get(0)?;
                    current.history_hash = row.get(1)?;
                    current.history_len = usize::try_from(row.get::<_, i64>(2)?).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    current.instructions_hash = row.get(3)?;
                    current.ready = row.get(4)?;
                    current.created_at = row.get(5)?;
                    current.last_used_at = row.get(6)?;
                    Ok(current)
                },
            ).optional()
        }).await
    }

    pub async fn cleanup(&self, active: HashSet<String>) -> Result<Vec<String>, ApiError> {
        self.run(move |connection| {
            let transaction = connection.transaction()?;
            let expired: Vec<String> = {
                let mut query =
                    transaction.prepare("SELECT id FROM sessions WHERE last_used_at<?1")?;
                query
                    .query_map([now() - RETENTION], |row| row.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            let mut deleted = Vec::new();
            for id in expired {
                if !active.contains(&id)
                    && transaction.execute(
                        "DELETE FROM sessions WHERE id=?1 AND last_used_at<?2",
                        params![id, now() - RETENTION],
                    )? > 0
                {
                    deleted.push(id);
                }
            }
            transaction.commit()?;
            Ok(deleted)
        })
        .await
    }
}
