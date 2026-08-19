use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use rusqlite::{Connection, params};

#[derive(Clone)]
pub struct ClaimStore {
    inner: Arc<DashMap<String, String>>,
    db_path: Option<PathBuf>,
}

impl ClaimStore {
    pub fn open(db_path: Option<PathBuf>) -> Result<Self, String> {
        let inner = Arc::new(DashMap::new());
        if let Some(ref path) = db_path {
            load_sqlite(path, &inner)?;
        }
        Ok(Self { inner, db_path })
    }

    #[cfg(test)]
    pub fn in_memory() -> Self {
        Self {
            inner: Arc::new(DashMap::new()),
            db_path: None,
        }
    }

    pub fn requires_machine_id(&self) -> bool {
        self.db_path.is_some()
    }

    pub fn assert_owner(&self, subdomain: &str, machine_id: &str) -> Result<(), String> {
        if machine_id.is_empty() {
            return Err("this Vyse edge requires a machine id".into());
        }
        if let Some(existing) = self.inner.get(subdomain) {
            if existing.value() == machine_id {
                return Ok(());
            }
            return Err(format!("subdomain `{subdomain}` is bound to another machine"));
        }

        self.inner
            .insert(subdomain.to_string(), machine_id.to_string());
        if let Some(ref path) = self.db_path {
            persist_claim(path, subdomain, machine_id)?;
        }
        Ok(())
    }

    pub fn owner(&self, subdomain: &str) -> Option<String> {
        self.inner.get(subdomain).map(|entry| entry.value().clone())
    }

    pub fn is_available_for(&self, subdomain: &str, machine_id: &str) -> bool {
        match self.inner.get(subdomain) {
            None => true,
            Some(existing) => existing.value() == machine_id,
        }
    }
}

fn load_sqlite(path: &Path, inner: &DashMap<String, String>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let conn = Connection::open(path).map_err(|err| err.to_string())?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS claims (
            subdomain TEXT PRIMARY KEY,
            machine_id TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
        [],
    )
    .map_err(|err| err.to_string())?;

    let mut stmt = conn
        .prepare("SELECT subdomain, machine_id FROM claims")
        .map_err(|err| err.to_string())?;
    let rows = stmt
        .query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))
        .map_err(|err| err.to_string())?;
    for row in rows {
        let (subdomain, machine_id) = row.map_err(|err| err.to_string())?;
        inner.insert(subdomain, machine_id);
    }
    Ok(())
}

fn persist_claim(path: &Path, subdomain: &str, machine_id: &str) -> Result<(), String> {
    let conn = Connection::open(path).map_err(|err| err.to_string())?;
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|err| err.to_string())?
        .as_secs() as i64;
    conn.execute(
        "INSERT INTO claims (subdomain, machine_id, created_at) VALUES (?1, ?2, ?3)",
        params![subdomain, machine_id, created_at],
    )
    .map_err(|err| err.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_claim_wins_in_memory() {
        let claims = ClaimStore::in_memory();
        claims.assert_owner("demo", "hw-a").unwrap();
        assert_eq!(claims.owner("demo"), Some("hw-a".into()));
    }

    #[test]
    fn same_id_reconnects_in_memory() {
        let claims = ClaimStore::in_memory();
        claims.assert_owner("demo", "hw-a").unwrap();
        claims.assert_owner("demo", "hw-a").unwrap();
    }

    #[test]
    fn other_id_rejected_in_memory() {
        let claims = ClaimStore::in_memory();
        claims.assert_owner("demo", "hw-a").unwrap();
        let err = claims.assert_owner("demo", "hw-b").unwrap_err();
        assert_eq!(err, "subdomain `demo` is bound to another machine");
    }

    #[test]
    fn empty_machine_id_rejected() {
        let claims = ClaimStore::in_memory();
        let err = claims.assert_owner("demo", "").unwrap_err();
        assert_eq!(err, "this Vyse edge requires a machine id");
    }

    #[test]
    fn sqlite_persists_claims() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("claims.db");

        {
            let claims = ClaimStore::open(Some(path.clone())).unwrap();
            claims.assert_owner("demo", "hw-a").unwrap();
        }

        let claims = ClaimStore::open(Some(path)).unwrap();
        assert_eq!(claims.owner("demo"), Some("hw-a".into()));
        claims.assert_owner("demo", "hw-a").unwrap();
        assert!(claims.assert_owner("demo", "hw-b").is_err());
    }
}
