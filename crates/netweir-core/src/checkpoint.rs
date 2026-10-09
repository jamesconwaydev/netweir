//! A crawl's state on disk, so a crawl killed at any point resumes where it
//! stopped.
//!
//! SQLite in WAL mode does the crash safety. Requests are plain data; the
//! caller's own part of a request (callback names, meta) is an opaque JSON
//! string. Writes go through one thread that commits them in order, a batch
//! at a time, so after a crash the file holds everything up to some point
//! and nothing after it: a request is never marked done unless the requests
//! it led to were saved first. Delivery is at least once.

use std::collections::HashSet;
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, params};

use crate::canonical::Fingerprint;

/// The layout this build writes. Older files are upgraded on open; newer
/// ones are refused.
pub const FORMAT: i64 = 1;

/// A request that was saved and not finished.
#[derive(Debug, Clone, PartialEq)]
pub struct Pending {
    pub row: i64,
    pub url: String,
    pub priority: i32,
    pub headers: Vec<(String, String)>,
    pub dont_filter: bool,
    pub depth: u32,
    pub payload: String,
}

/// What a checkpoint held when it was opened.
#[derive(Debug, Default)]
pub struct Saved {
    pub pending: Vec<Pending>,
    pub seen: HashSet<Fingerprint>,
    pub items: HashSet<String>,
    /// The caller's counters, as it last saved them (JSON).
    pub counters: Option<String>,
    /// The next free row number.
    pub next_row: i64,
}

enum Op {
    Request(Pending),
    Seen(Fingerprint),
    Done(i64),
    Item(String),
    Counters(String),
    /// Items and counters that must land in the same commit.
    Settle(Vec<String>, String),
    /// Commit what's queued and reply.
    Flush(Sender<()>),
}

/// The open checkpoint. Writes are queued and committed by its thread;
/// dropping it commits what's left.
pub struct Checkpoint {
    ops: Option<Sender<Op>>,
    writer: Option<JoinHandle<()>>,
    /// The first write that failed since the last flush.
    failed: Arc<Mutex<Option<String>>>,
}

#[derive(Debug)]
pub struct CheckpointError(pub String);

impl std::fmt::Display for CheckpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CheckpointError {}

fn sql(e: rusqlite::Error) -> CheckpointError {
    CheckpointError(format!("checkpoint: {e}"))
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
    CREATE TABLE IF NOT EXISTS requests (
        row INTEGER PRIMARY KEY,
        url TEXT NOT NULL,
        priority INTEGER NOT NULL,
        headers TEXT NOT NULL,
        dont_filter INTEGER NOT NULL,
        depth INTEGER NOT NULL,
        payload TEXT NOT NULL,
        done INTEGER NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS pending ON requests (row) WHERE done = 0;
    CREATE TABLE IF NOT EXISTS seen (fingerprint BLOB PRIMARY KEY) WITHOUT ROWID;
    CREATE TABLE IF NOT EXISTS items (id TEXT PRIMARY KEY) WITHOUT ROWID;
";

impl Checkpoint {
    /// Opens or creates the checkpoint at `path` and reads back what it
    /// holds.
    pub fn open(path: &Path) -> Result<(Checkpoint, Saved), CheckpointError> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)
                .map_err(|e| CheckpointError(format!("can't create {}: {e}", dir.display())))?;
        }
        let conn = Connection::open(path).map_err(sql)?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(sql)?;
        // WAL with NORMAL is crash-safe for the process dying; a power cut
        // may lose the last commits, which resuming fetches again.
        conn.pragma_update(None, "synchronous", "NORMAL")
            .map_err(sql)?;
        conn.execute_batch(SCHEMA).map_err(sql)?;
        let format: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'format'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(sql)?;
        match format.as_deref().map(str::parse::<i64>) {
            None => {
                conn.execute(
                    "INSERT INTO meta (key, value) VALUES ('format', ?1)",
                    params![FORMAT.to_string()],
                )
                .map_err(sql)?;
            }
            Some(Ok(v)) if v <= FORMAT => {}
            Some(Ok(v)) => {
                return Err(CheckpointError(format!(
                    "{} was written by a newer netweir (format {v}; this one reads up to {FORMAT})",
                    path.display()
                )));
            }
            Some(Err(_)) => {
                return Err(CheckpointError(format!(
                    "{} is not a netweir checkpoint",
                    path.display()
                )));
            }
        }
        let saved = read(&conn).map_err(sql)?;
        let (ops, inbox) = channel();
        let failed = Arc::new(Mutex::new(None));
        let writer_failed = failed.clone();
        let writer = std::thread::Builder::new()
            .name("netweir-checkpoint".into())
            .spawn(move || write_loop(conn, inbox, writer_failed))
            .map_err(|e| CheckpointError(format!("can't start the checkpoint writer: {e}")))?;
        Ok((
            Checkpoint {
                ops: Some(ops),
                writer: Some(writer),
                failed,
            },
            saved,
        ))
    }

    fn send(&self, op: Op) {
        if let Some(ops) = &self.ops {
            // The writer only stops when we drop the sender.
            let _ = ops.send(op);
        }
    }

    pub fn request(&self, pending: Pending) {
        self.send(Op::Request(pending));
    }

    pub fn seen(&self, fp: Fingerprint) {
        self.send(Op::Seen(fp));
    }

    pub fn done(&self, row: i64) {
        self.send(Op::Done(row));
    }

    pub fn item(&self, id: String) {
        self.send(Op::Item(id));
    }

    pub fn counters(&self, json: String) {
        self.send(Op::Counters(json));
    }

    /// Records items as delivered together with the counters that go with
    /// them (such as how long the output files were), in one commit: after
    /// a crash, either both are saved or neither is.
    pub fn settle(&self, items: Vec<String>, counters: String) {
        self.send(Op::Settle(items, counters));
    }

    /// Waits until everything queued so far is committed. An error if a
    /// write since the last flush failed; that batch is not in the file,
    /// so a resume would do its work again.
    pub fn flush(&self) -> Result<(), String> {
        let (tx, rx) = channel();
        self.send(Op::Flush(tx));
        let _ = rx.recv();
        match self.failed.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

impl Drop for Checkpoint {
    fn drop(&mut self) {
        drop(self.ops.take());
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
    }
}

fn read(conn: &Connection) -> rusqlite::Result<Saved> {
    let mut saved = Saved::default();
    let mut stmt = conn.prepare(
        "SELECT row, url, priority, headers, dont_filter, depth, payload
         FROM requests WHERE done = 0 ORDER BY row",
    )?;
    let rows = stmt.query_map([], |r| {
        let headers: String = r.get(3)?;
        Ok(Pending {
            row: r.get(0)?,
            url: r.get(1)?,
            priority: r.get(2)?,
            headers: serde_json::from_str(&headers).unwrap_or_default(),
            dont_filter: r.get(4)?,
            depth: r.get(5)?,
            payload: r.get(6)?,
        })
    })?;
    for row in rows {
        saved.pending.push(row?);
    }
    let mut stmt = conn.prepare("SELECT fingerprint FROM seen")?;
    for fp in stmt.query_map([], |r| r.get::<_, Vec<u8>>(0))? {
        if let Ok(fp) = Fingerprint::try_from(fp?.as_slice()) {
            saved.seen.insert(fp);
        }
    }
    let mut stmt = conn.prepare("SELECT id FROM items")?;
    for id in stmt.query_map([], |r| r.get::<_, String>(0))? {
        saved.items.insert(id?);
    }
    saved.counters = conn
        .query_row("SELECT value FROM meta WHERE key = 'counters'", [], |r| {
            r.get(0)
        })
        .optional()?;
    saved.next_row = conn.query_row("SELECT COALESCE(MAX(row), 0) + 1 FROM requests", [], |r| {
        r.get(0)
    })?;
    Ok(saved)
}

/// Commits the queued writes in order, a batch per transaction.
fn write_loop(mut conn: Connection, inbox: Receiver<Op>, failed: Arc<Mutex<Option<String>>>) {
    // Waits for the first write, then takes whatever else arrives within
    // this long, so a busy crawl commits in batches rather than per row.
    const GATHER: Duration = Duration::from_millis(50);
    while let Ok(first) = inbox.recv() {
        let mut batch = vec![first];
        let until = std::time::Instant::now() + GATHER;
        while batch.len() < 10_000 {
            let left = until.saturating_duration_since(std::time::Instant::now());
            match inbox.recv_timeout(left) {
                Ok(op) => batch.push(op),
                Err(_) => break,
            }
        }
        // Flushes are answered after the commit's result is known, even
        // when it fails part way.
        let (flushes, writes): (Vec<Op>, Vec<Op>) =
            batch.into_iter().partition(|op| matches!(op, Op::Flush(_)));
        if let Err(e) = commit(&mut conn, writes) {
            // The crawl goes on; the next flush reports it, and a resume
            // would do this batch's work again.
            failed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_or_insert_with(|| format!("checkpoint write failed: {e}"));
        }
        for op in flushes {
            if let Op::Flush(reply) = op {
                let _ = reply.send(());
            }
        }
    }
}

fn commit(conn: &mut Connection, batch: Vec<Op>) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut request = tx.prepare_cached(
            "INSERT OR REPLACE INTO requests (row, url, priority, headers, dont_filter, depth, payload)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        let mut seen = tx.prepare_cached("INSERT OR IGNORE INTO seen (fingerprint) VALUES (?1)")?;
        let mut done = tx.prepare_cached("UPDATE requests SET done = 1 WHERE row = ?1")?;
        let mut item = tx.prepare_cached("INSERT OR IGNORE INTO items (id) VALUES (?1)")?;
        let mut counters = tx.prepare_cached(
            "INSERT INTO meta (key, value) VALUES ('counters', ?1)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
        )?;
        for op in batch {
            match op {
                Op::Request(p) => {
                    let headers = serde_json::to_string(&p.headers).unwrap_or_else(|_| "[]".into());
                    request.execute(params![
                        p.row,
                        p.url,
                        p.priority,
                        headers,
                        p.dont_filter,
                        p.depth,
                        p.payload
                    ])?;
                }
                Op::Seen(fp) => {
                    seen.execute(params![fp.as_slice()])?;
                }
                Op::Done(row) => {
                    done.execute(params![row])?;
                }
                Op::Item(id) => {
                    item.execute(params![id])?;
                }
                Op::Counters(json) => {
                    counters.execute(params![json])?;
                }
                Op::Settle(ids, json) => {
                    for id in ids {
                        item.execute(params![id])?;
                    }
                    counters.execute(params![json])?;
                }
                Op::Flush(_) => {}
            }
        }
    }
    tx.commit()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(row: i64, url: &str) -> Pending {
        Pending {
            row,
            url: url.into(),
            priority: 2,
            headers: vec![("x-a".into(), "1".into())],
            dont_filter: false,
            depth: 3,
            payload: r#"{"callback":"parse"}"#.into(),
        }
    }

    #[test]
    fn what_was_written_comes_back_and_done_requests_do_not() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        {
            let (cp, saved) = Checkpoint::open(&path).unwrap();
            assert!(saved.pending.is_empty());
            assert_eq!(saved.next_row, 1);
            cp.request(pending(1, "https://e.com/a"));
            cp.request(pending(2, "https://e.com/b"));
            cp.seen([7; 16]);
            cp.done(1);
            cp.item("abc".into());
            cp.counters(r#"{"items":1}"#.into());
        }
        let (_cp, saved) = Checkpoint::open(&path).unwrap();
        assert_eq!(saved.pending, vec![pending(2, "https://e.com/b")]);
        assert!(saved.seen.contains(&[7; 16]));
        assert!(saved.items.contains("abc"));
        assert_eq!(saved.counters.as_deref(), Some(r#"{"items":1}"#));
        assert_eq!(saved.next_row, 3);
        drop(_cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_write_is_reported_by_flush() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-fail-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        let (cp, _) = Checkpoint::open(&path).unwrap();
        cp.flush().unwrap();
        Connection::open(&path)
            .unwrap()
            .execute("DROP TABLE requests", [])
            .unwrap();
        cp.request(pending(1, "https://e.com/a"));
        let err = cp.flush().unwrap_err();
        assert!(err.contains("requests"), "{err}");
        assert!(cp.flush().is_ok(), "reported once");
        drop(cp);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_newer_format_is_refused() {
        let dir = std::env::temp_dir().join(format!("netweir-cp-new-{}", std::process::id()));
        let path = dir.join("crawl.sqlite3");
        let _ = std::fs::remove_dir_all(&dir);
        drop(Checkpoint::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        conn.execute("UPDATE meta SET value = '99' WHERE key = 'format'", [])
            .unwrap();
        drop(conn);
        let err = Checkpoint::open(&path).err().unwrap();
        assert!(err.to_string().contains("newer netweir"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
