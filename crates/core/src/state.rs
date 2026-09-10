pub use crate::config::{HeaderKind, HeaderMode};
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Mutex};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgressRow {
    pub file_key: String,
    pub path: String,
    pub display_path: String,
    pub name_seg: String,
    pub offset: u64,
    pub size: u64,
    pub header_mode: HeaderMode,
    pub header: Option<Vec<u8>>,
    pub header_kind: Option<HeaderKind>,
    pub field_count: Option<u32>,
    pub last_seen: String,
    pub updated_at: String,
    pub last_error: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: i64,
    pub task_name: String,
    pub started_at: String,
    pub finished_at: Option<String>,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub skipped: bool,
    pub duration_ms: Option<u64>,
    pub stdout_tail: Option<String>,
    pub stderr_tail: Option<String>,
}
pub struct StateDb {
    conn: Mutex<Connection>,
    pub recovery_warning: Option<String>,
}
impl StateDb {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let attempt = || -> Result<Connection> {
            let c = Connection::open(path)?;
            let check: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
            anyhow::ensure!(check == "ok", "SQLite integrity check: {check}");
            Ok(c)
        };
        let (conn, recovery_warning) = match attempt() {
            Ok(c) => (c, None),
            Err(e) => {
                // Only quarantine proven corruption. Permission/locking errors must never reset valid state.
                let corrupt=e.downcast_ref::<rusqlite::Error>().is_some_and(|e|matches!(e,rusqlite::Error::SqliteFailure(code,_) if matches!(code.code,rusqlite::ErrorCode::DatabaseCorrupt|rusqlite::ErrorCode::NotADatabase))) || e.to_string().starts_with("SQLite integrity check:");
                if !corrupt || !path.exists() {
                    return Err(e);
                }
                let stamp = chrono::Local::now().format("%Y%m%d%H%M%S%f").to_string();
                let backup = path.with_extension(format!("db.corrupt-{stamp}"));
                std::fs::rename(path, &backup)?;
                for suffix in ["-wal", "-shm"] {
                    let side = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
                    if side.exists() {
                        std::fs::rename(
                            &side,
                            std::path::PathBuf::from(format!("{}{suffix}", backup.display())),
                        )?;
                    }
                }
                let warning = format!(
                    "state database recovered from corruption; backed up at {}: {e}",
                    backup.display()
                );
                tracing::error!("{warning}");
                (Connection::open(path)?, Some(warning))
            }
        };
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;
   CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);
   CREATE TABLE IF NOT EXISTS progress(file_key TEXT PRIMARY KEY,path TEXT NOT NULL,display_path TEXT NOT NULL,name_seg TEXT NOT NULL,offset INTEGER NOT NULL,size INTEGER NOT NULL,header_mode TEXT NOT NULL,header BLOB,header_kind TEXT,field_count INTEGER,last_seen TEXT NOT NULL,updated_at TEXT NOT NULL,last_error TEXT);
   CREATE INDEX IF NOT EXISTS progress_path ON progress(path);
   CREATE TABLE IF NOT EXISTS task_history(id INTEGER PRIMARY KEY AUTOINCREMENT,task_name TEXT NOT NULL,started_at TEXT NOT NULL,finished_at TEXT,exit_code INTEGER,timed_out INTEGER DEFAULT 0,skipped INTEGER DEFAULT 0,duration_ms INTEGER,stdout_tail TEXT,stderr_tail TEXT);
   CREATE INDEX IF NOT EXISTS task_history_name ON task_history(task_name,id DESC);
   CREATE TABLE IF NOT EXISTS counters(key TEXT PRIMARY KEY,value TEXT NOT NULL);")?;
        let version: Option<String> = conn
            .query_row(
                "SELECT value FROM meta WHERE key='schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        anyhow::ensure!(
            version.as_deref().is_none_or(|v| v == "1"),
            "unsupported database schema version {:?}",
            version
        );
        conn.execute(
            "INSERT OR IGNORE INTO meta(key,value)VALUES('schema_version','1')",
            [],
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
            recovery_warning,
        })
    }
    pub fn load_all_progress(&self) -> Result<Vec<ProgressRow>> {
        let c = self.conn.lock().unwrap();
        let mut q = c.prepare("SELECT * FROM progress ORDER BY path")?;
        let rows = q
            .query_map([], read_progress)?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    pub fn get_progress(&self, key: &str) -> Result<Option<ProgressRow>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT * FROM progress WHERE file_key=?1",
                [key],
                read_progress,
            )
            .optional()?)
    }
    pub fn upsert_progress(&self, r: &ProgressRow) -> Result<()> {
        self.conn.lock().unwrap().execute("INSERT INTO progress(file_key,path,display_path,name_seg,offset,size,header_mode,header,header_kind,field_count,last_seen,updated_at,last_error) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13) ON CONFLICT(file_key) DO UPDATE SET path=excluded.path,display_path=excluded.display_path,name_seg=excluded.name_seg,offset=excluded.offset,size=excluded.size,header_mode=excluded.header_mode,header=excluded.header,header_kind=excluded.header_kind,field_count=excluded.field_count,last_seen=excluded.last_seen,updated_at=excluded.updated_at,last_error=excluded.last_error",params![r.file_key,r.path,r.display_path,r.name_seg,r.offset,r.size,mode_name(r.header_mode),r.header,r.header_kind.map(HeaderKind::as_str),r.field_count,r.last_seen,r.updated_at,r.last_error])?;
        Ok(())
    }
    pub fn commit_offset(&self, row: &ProgressRow) -> Result<()> {
        self.upsert_progress(row)
    }
    pub fn delete_progress_by_path(&self, path: &str) -> Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM progress WHERE path=?1", [path])?;
        Ok(())
    }
    pub fn reset_offset(&self, key: &str) -> Result<Option<ProgressRow>> {
        self.conn.lock().unwrap().execute("UPDATE progress SET offset=0,header=NULL,header_kind=NULL,field_count=NULL,last_error=NULL WHERE file_key=?1",[key])?;
        self.get_progress(key)
    }
    /// Ownership survives progress deletion so recreated sources cannot overwrite prior generations.
    pub fn allocate_name(&self, base: &str, file_key: &str, path: &str) -> Result<String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let source = format!("source_name:{file_key}:{path}");
        let existing: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key=?1", [&source], |r| {
                r.get(0)
            })
            .optional()?;
        if let Some(name) = existing {
            return Ok(name);
        }
        let owner = format!("name_owner:{base}");
        let generation: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key=?1", [&owner], |r| {
                r.get(0)
            })
            .optional()?;
        let generation = generation
            .and_then(|s| s.parse::<u64>().ok())
            .map_or(0, |v| v.saturating_add(1));
        let name = if generation == 0 {
            base.to_owned()
        } else {
            format!(
                "{base}~g{generation}-{:016x}",
                crate::paths::stable_hash(format!("{file_key}:{path}").as_bytes())
            )
        };
        for (key, value) in [
            (owner, generation.to_string()),
            (source, name.clone()),
            (format!("source_base:{file_key}:{path}"), base.to_owned()),
        ] {
            tx.execute("INSERT INTO meta(key,value)VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,value])?;
        }
        tx.commit()?;
        Ok(name)
    }
    pub fn rotate_name(&self, row: &ProgressRow) -> Result<String> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let base_key = format!("source_base:{}:{}", row.file_key, row.path);
        let base: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key=?1", [&base_key], |r| {
                r.get(0)
            })
            .optional()?;
        let base = base.unwrap_or_else(|| row.name_seg.clone());
        let owner = format!("name_owner:{base}");
        let generation: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key=?1", [&owner], |r| {
                r.get(0)
            })
            .optional()?;
        let generation = generation
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_add(1);
        let name = format!(
            "{base}~g{generation}-{:016x}",
            crate::paths::stable_hash(format!("{}:{}", row.file_key, row.path).as_bytes())
        );
        for (key, value) in [
            (owner, generation.to_string()),
            (
                format!("source_name:{}:{}", row.file_key, row.path),
                name.clone(),
            ),
            (base_key, base),
        ] {
            tx.execute("INSERT INTO meta(key,value)VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[key,value])?;
        }
        tx.commit()?;
        Ok(name)
    }
    pub fn insert_task_run(&self, r: &TaskRun) -> Result<i64> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("INSERT INTO task_history(task_name,started_at,finished_at,exit_code,timed_out,skipped,duration_ms,stdout_tail,stderr_tail)VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",params![r.task_name,r.started_at,r.finished_at,r.exit_code,r.timed_out,r.skipped,r.duration_ms,r.stdout_tail,r.stderr_tail])?;
        let id = tx.last_insert_rowid();
        tx.execute("DELETE FROM task_history WHERE task_name=?1 AND id NOT IN(SELECT id FROM task_history WHERE task_name=?1 ORDER BY id DESC LIMIT 100)",[&r.task_name])?;
        tx.commit()?;
        Ok(id)
    }
    pub fn finish_task_run(&self, r: &TaskRun) -> Result<()> {
        self.conn.lock().unwrap().execute("UPDATE task_history SET finished_at=?2,exit_code=?3,timed_out=?4,skipped=?5,duration_ms=?6,stdout_tail=?7,stderr_tail=?8 WHERE id=?1",params![r.id,r.finished_at,r.exit_code,r.timed_out,r.skipped,r.duration_ms,r.stdout_tail,r.stderr_tail])?;
        Ok(())
    }
    pub fn recent_task_runs(&self, name: &str, n: usize) -> Result<Vec<TaskRun>> {
        let c = self.conn.lock().unwrap();
        let mut q =
            c.prepare("SELECT * FROM task_history WHERE task_name=?1 ORDER BY id DESC LIMIT ?2")?;
        let rows = q
            .query_map(params![name, n.min(100) as u32], |r| {
                Ok(TaskRun {
                    id: r.get(0)?,
                    task_name: r.get(1)?,
                    started_at: r.get(2)?,
                    finished_at: r.get(3)?,
                    exit_code: r.get(4)?,
                    timed_out: r.get(5)?,
                    skipped: r.get(6)?,
                    duration_ms: r.get(7)?,
                    stdout_tail: r.get(8)?,
                    stderr_tail: r.get(9)?,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }
    fn get_value(&self, table: &str, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .lock()
            .unwrap()
            .query_row(
                &format!("SELECT value FROM {table} WHERE key=?1"),
                [key],
                |r| r.get(0),
            )
            .optional()?)
    }
    fn set_value(&self, table: &str, key: &str, val: &str) -> Result<()> {
        self.conn.lock().unwrap().execute(&format!("INSERT INTO {table}(key,value)VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value"),[key,val])?;
        Ok(())
    }
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        self.get_value("meta", key)
    }
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.set_value("meta", key, value)
    }
    pub fn get_counter(&self, key: &str) -> Result<Option<String>> {
        self.get_value("counters", key)
    }
    pub fn set_counter(&self, key: &str, value: &str) -> Result<()> {
        self.set_value("counters", key, value)
    }
}
fn mode_name(m: HeaderMode) -> &'static str {
    match m {
        HeaderMode::Auto => "auto",
        HeaderMode::None => "none",
        HeaderMode::Fixed => "fixed",
    }
}
fn read_progress(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProgressRow> {
    Ok(ProgressRow {
        file_key: r.get(0)?,
        path: r.get(1)?,
        display_path: r.get(2)?,
        name_seg: r.get(3)?,
        offset: r.get(4)?,
        size: r.get(5)?,
        header_mode: match r.get::<_, String>(6)?.as_str() {
            "none" => HeaderMode::None,
            "fixed" => HeaderMode::Fixed,
            _ => HeaderMode::Auto,
        },
        header: r.get(7)?,
        header_kind: r.get::<_, Option<String>>(8)?.map(|s| {
            if s == "hdr" {
                HeaderKind::Hdr
            } else {
                HeaderKind::Syn
            }
        }),
        field_count: r.get(9)?,
        last_seen: r.get(10)?,
        updated_at: r.get(11)?,
        last_error: r.get(12)?,
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn source_generation_survives_progress_deletion_and_restart() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.db");
        let db = StateDb::open(&p).unwrap();
        assert_eq!(
            db.allocate_name("data.csv", "key1", "c:/data.csv").unwrap(),
            "data.csv"
        );
        db.delete_progress_by_path("c:/data.csv").unwrap();
        drop(db);
        let db = StateDb::open(&p).unwrap();
        let second = db.allocate_name("data.csv", "key2", "c:/data.csv").unwrap();
        assert_ne!(second, "data.csv");
        assert_eq!(
            db.allocate_name("data.csv", "key2", "c:/data.csv").unwrap(),
            second
        );
    }
    #[test]
    fn corruption_recovery_and_task_retention() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("state.db");
        std::fs::write(&p, b"broken database content").unwrap();
        let db = StateDb::open(&p).unwrap();
        assert!(db.recovery_warning.is_some());
        for _ in 0..105 {
            db.insert_task_run(&TaskRun {
                task_name: "x".into(),
                started_at: "now".into(),
                ..Default::default()
            })
            .unwrap();
        }
        assert_eq!(db.recent_task_runs("x", 200).unwrap().len(), 100);
        db.set_meta("hello", "world").unwrap();
        assert_eq!(db.get_meta("hello").unwrap().as_deref(), Some("world"));
    }
}
