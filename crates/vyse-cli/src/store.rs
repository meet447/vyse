use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use vyse_core::http::{content_length, request_method, request_path};

const MAX_ROWS: usize = 500;

#[derive(Debug, Clone)]
pub struct LoggedRequest {
    pub id: String,
    pub created_at: i64,
    pub method: String,
    pub path: String,
    pub port: u16,
    pub headers: String,
    pub body: Vec<u8>,
    pub status: Option<u16>,
}

#[derive(Clone)]
pub struct RequestStore {
    db: Arc<Mutex<Connection>>,
}

impl RequestStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create {}", parent.display()))?;
        }
        let db = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS requests (
                id TEXT PRIMARY KEY,
                created_at INTEGER NOT NULL,
                method TEXT NOT NULL,
                path TEXT NOT NULL,
                port INTEGER NOT NULL,
                headers TEXT NOT NULL,
                body BLOB NOT NULL,
                status INTEGER
            );
            CREATE INDEX IF NOT EXISTS requests_created_at ON requests(created_at DESC);",
        )?;
        Ok(Self {
            db: Arc::new(Mutex::new(db)),
        })
    }

    pub fn default_path() -> PathBuf {
        directories::ProjectDirs::from("dev", "Vyse", "vyse")
            .map(|dirs| dirs.data_local_dir().join("webhooks.db"))
            .unwrap_or_else(|| PathBuf::from("vyse-webhooks.db"))
    }

    pub fn insert(
        &self,
        port: u16,
        raw_request: &[u8],
        status: Option<u16>,
    ) -> Result<LoggedRequest> {
        let created_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let id = format!("{created_at:x}");
        let method = request_method(raw_request).unwrap_or_else(|| "GET".into());
        let path = request_path(raw_request).unwrap_or_else(|| "/".into());
        let headers = headers_only(raw_request);
        let body = body_of(raw_request);
        let row = LoggedRequest {
            id: id.clone(),
            created_at,
            method: method.clone(),
            path: path.clone(),
            port,
            headers: headers.clone(),
            body: body.clone(),
            status,
        };
        let db = self.db.lock().expect("request store mutex");
        db.execute(
            "INSERT INTO requests (id, created_at, method, path, port, headers, body, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                row.id,
                row.created_at,
                row.method,
                row.path,
                row.port,
                row.headers,
                row.body,
                row.status.map(|s| s as i64)
            ],
        )?;
        db.execute(
            "DELETE FROM requests WHERE id NOT IN (
                SELECT id FROM requests ORDER BY created_at DESC LIMIT ?1
            )",
            params![MAX_ROWS as i64],
        )?;
        Ok(row)
    }

    pub fn get(&self, id: &str) -> Result<Option<LoggedRequest>> {
        let db = self.db.lock().expect("request store mutex");
        let mut stmt = db.prepare(
            "SELECT id, created_at, method, path, port, headers, body, status
             FROM requests WHERE id = ?1",
        )?;
        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(map_row(row)?)),
            None => Ok(None),
        }
    }

    pub fn recent(&self, limit: usize) -> Result<Vec<LoggedRequest>> {
        let db = self.db.lock().expect("request store mutex");
        let mut stmt = db.prepare(
            "SELECT id, created_at, method, path, port, headers, body, status
             FROM requests ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(LoggedRequest {
                id: row.get(0)?,
                created_at: row.get(1)?,
                method: row.get(2)?,
                path: row.get(3)?,
                port: row.get::<_, i64>(4)? as u16,
                headers: row.get(5)?,
                body: row.get(6)?,
                status: row.get::<_, Option<i64>>(7)?.map(|s| s as u16),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<LoggedRequest> {
    Ok(LoggedRequest {
        id: row.get(0)?,
        created_at: row.get(1)?,
        method: row.get(2)?,
        path: row.get(3)?,
        port: row.get::<_, i64>(4)? as u16,
        headers: row.get(5)?,
        body: row.get(6)?,
        status: row.get::<_, Option<i64>>(7)?.map(|s| s as u16),
    })
}

fn headers_only(raw: &[u8]) -> String {
    if let Some(idx) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        String::from_utf8_lossy(&raw[..idx]).into_owned()
    } else {
        String::from_utf8_lossy(raw).into_owned()
    }
}

fn body_of(raw: &[u8]) -> Vec<u8> {
    if let Some(idx) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
        let start = idx + 4;
        let len = content_length(raw).unwrap_or(raw.len().saturating_sub(start));
        raw[start..start.saturating_add(len).min(raw.len())].to_vec()
    } else {
        Vec::new()
    }
}
