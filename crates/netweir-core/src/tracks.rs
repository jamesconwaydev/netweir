//! Where tracked selectors keep their fingerprints: a table in an SQLite
//! file (a crawl's checkpoint, or ~/.netweir/tracks.db), keyed by site and
//! name.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension, params};

use crate::checkpoint::CheckpointError;

pub struct TrackStore {
    /// None once closed.
    conn: Mutex<Option<Connection>>,
    /// What's in the table, as read or written by this process, so a
    /// fingerprint that hasn't changed isn't written again on every page.
    cache: Mutex<HashMap<(String, String), String>>,
}

impl TrackStore {
    pub fn open(path: &Path) -> Result<TrackStore, CheckpointError> {
        let err = |e: rusqlite::Error| CheckpointError(format!("{}: {e}", path.display()));
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| CheckpointError(format!("can't create {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path).map_err(err)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(err)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS tracks (
                site TEXT NOT NULL,
                name TEXT NOT NULL,
                fingerprint TEXT NOT NULL,
                PRIMARY KEY (site, name)
            ) WITHOUT ROWID;",
        )
        .map_err(err)?;
        Ok(TrackStore {
            conn: Mutex::new(Some(conn)),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn get(&self, site: &str, name: &str) -> Option<String> {
        let key = (site.to_string(), name.to_string());
        if let Some(fp) = self
            .cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return Some(fp.clone());
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let fp: Option<String> = conn
            .as_ref()?
            .query_row(
                "SELECT fingerprint FROM tracks WHERE site = ?1 AND name = ?2",
                params![site, name],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        if let Some(fp) = &fp {
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, fp.clone());
        }
        fp
    }

    /// Saves the fingerprint, unless it's what's saved already. A failed
    /// write is not an error for the crawl: the old fingerprint stays.
    pub fn put(&self, site: &str, name: &str, fingerprint: &str) {
        let key = (site.to_string(), name.to_string());
        {
            let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
            if cache.get(&key).is_some_and(|fp| fp == fingerprint) {
                return;
            }
        }
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let Some(conn) = conn.as_ref() else {
            return;
        };
        let written = conn.execute(
            "INSERT INTO tracks (site, name, fingerprint) VALUES (?1, ?2, ?3)
             ON CONFLICT (site, name) DO UPDATE SET fingerprint = excluded.fingerprint",
            params![site, name, fingerprint],
        );
        if written.is_ok() {
            self.cache
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(key, fingerprint.to_string());
        }
    }

    /// Closes the file. Fingerprints already read stay available; nothing
    /// more is read or saved.
    pub fn close(&self) {
        self.conn.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_persist_by_site_and_name() {
        let dir = std::env::temp_dir().join(format!("netweir-tracks-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("tracks.db");
        {
            let store = TrackStore::open(&path).unwrap();
            assert_eq!(store.get("e.com", "price"), None);
            store.put("e.com", "price", "{\"a\":1}");
            store.put("e.com", "price", "{\"a\":1}");
            store.put("f.com", "price", "{\"b\":2}");
        }
        let store = TrackStore::open(&path).unwrap();
        assert_eq!(store.get("e.com", "price").as_deref(), Some("{\"a\":1}"));
        assert_eq!(store.get("f.com", "price").as_deref(), Some("{\"b\":2}"));
        assert_eq!(store.get("e.com", "title"), None);
        store.close();
        assert_eq!(store.get("e.com", "price").as_deref(), Some("{\"a\":1}"));
        assert_eq!(store.get("g.com", "price"), None);
        store.put("g.com", "price", "{}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
