use crate::{
    config::HeaderKind,
    paths::{format_chunk_name, parse_chunk_name, ChunkName},
};
use anyhow::Result;
use std::{
    fs::{self, File},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::Mutex,
    time::SystemTime,
};
#[derive(Debug, Clone, Copy, Default)]
pub struct SpoolStats {
    pub pending: u64,
    pub bytes: u64,
}
#[derive(Debug, Clone)]
pub struct SpoolItem {
    pub path: PathBuf,
    pub date_dir: String,
    pub name: String,
    pub size: u64,
    pub mtime: SystemTime,
}
pub struct ChunkSpec<'a> {
    pub name_seg: &'a str,
    pub start: u64,
    pub end: u64,
    pub kind: HeaderKind,
    pub header: Option<&'a [u8]>,
    pub body: &'a [u8],
}
pub struct Spool {
    pub dir: PathBuf,
    lock: Mutex<()>,
}
impl Spool {
    pub fn new(dir: impl Into<PathBuf>) -> Result<Self> {
        let this = Self {
            dir: dir.into(),
            lock: Mutex::new(()),
        };
        this.init()?;
        Ok(this)
    }
    pub fn init(&self) -> Result<()> {
        let _lock = self.lock.lock().unwrap();
        fs::create_dir_all(&self.dir)?;
        for e in walkdir::WalkDir::new(&self.dir).min_depth(1).max_depth(2) {
            let e = e?;
            if e.file_type().is_file() && e.file_name().to_string_lossy().ends_with(".csv.gz.tmp") {
                fs::remove_file(e.path())?;
            }
        }
        Ok(())
    }
    fn list_unlocked(&self) -> Result<Vec<SpoolItem>> {
        let mut items = vec![];
        for e in walkdir::WalkDir::new(&self.dir).min_depth(2).max_depth(2) {
            let e = e?;
            if !e.file_type().is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().into_owned();
            if parse_chunk_name(&name).is_none() {
                continue;
            }
            let date_dir = e
                .path()
                .parent()
                .and_then(Path::file_name)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if date_dir.len() != 8 || !date_dir.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let m = e.metadata()?;
            items.push(SpoolItem {
                path: e.path().into(),
                date_dir,
                name,
                size: m.len(),
                mtime: m.modified()?,
            });
        }
        items.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
        Ok(items)
    }
    pub fn list_oldest_first(&self) -> Result<Vec<SpoolItem>> {
        let _lock = self.lock.lock().unwrap();
        self.list_unlocked()
    }
    pub fn stats(&self) -> Result<SpoolStats> {
        let _lock = self.lock.lock().unwrap();
        let items = self.list_unlocked()?;
        Ok(SpoolStats {
            pending: items.len() as u64,
            bytes: items.iter().map(|i| i.size).sum(),
        })
    }
    fn remove_unlocked(&self, item: &SpoolItem) -> Result<()> {
        match fs::remove_file(&item.path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        };
        if let Some(p) = item.path.parent() {
            match fs::remove_dir(p) {
                Ok(()) => {}
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::DirectoryNotEmpty | io::ErrorKind::NotFound
                    ) => {}
                Err(e) => tracing::debug!(error=%e,"could not remove spool directory"),
            }
        }
        Ok(())
    }
    pub fn remove(&self, item: &SpoolItem) -> Result<()> {
        let _lock = self.lock.lock().unwrap();
        self.remove_unlocked(item)
    }
    pub fn write_chunk(&self, spec: ChunkSpec<'_>, max_bytes: u64) -> Result<PathBuf> {
        let _lock = self.lock.lock().unwrap();
        anyhow::ensure!(
            !spec.name_seg.is_empty()
                && !spec.name_seg.contains(['\\', '/'])
                && spec.end > spec.start,
            "invalid chunk name/range"
        );
        let date = chrono::Local::now().format("%Y%m%d").to_string();
        let dir = self.dir.join(date);
        fs::create_dir_all(&dir)?;
        let name = format_chunk_name(&ChunkName {
            name_seg: spec.name_seg.into(),
            start: spec.start,
            end: spec.end,
            kind: spec.kind,
        });
        let mut path = dir.join(&name);
        let tmp = dir.join(format!("{name}.tmp"));
        let mut gzip =
            flate2::write::GzEncoder::new(File::create(&tmp)?, flate2::Compression::default());
        if let Some(header) = spec.header {
            gzip.write_all(header)?;
            gzip.write_all(b"\n")?;
        }
        gzip.write_all(spec.body)?;
        let f = gzip.finish()?;
        f.sync_all()?;
        drop(f);
        if path.exists() {
            let old = fs::read(&path)?;
            let new = fs::read(&tmp)?;
            if old != new {
                // Generation tracking normally prevents collisions; preserve both if an unexpected rewrite occurs.
                let suffix = crate::paths::stable_hash(&new);
                let mut attempt = 0u32;
                loop {
                    let extra = if attempt == 0 {
                        String::new()
                    } else {
                        format!("-{attempt}")
                    };
                    path = dir.join(format_chunk_name(&ChunkName {
                        name_seg: format!("{}~c{suffix:016x}{extra}", spec.name_seg),
                        start: spec.start,
                        end: spec.end,
                        kind: spec.kind,
                    }));
                    if !path.exists() || fs::read(&path)? == new {
                        break;
                    }
                    attempt = attempt
                        .checked_add(1)
                        .ok_or_else(|| anyhow::anyhow!("chunk collision namespace exhausted"))?;
                }
                tracing::warn!(original=%name,preserved_as=%path.display(),"chunk name collision; preserving previous payload");
            }
        }
        crate::win::replace_file(&tmp, &path)?;
        let items = self.list_unlocked()?;
        let mut bytes: u64 = items.iter().map(|i| i.size).sum();
        for item in items {
            if bytes <= max_bytes {
                break;
            }
            tracing::warn!(chunk=%item.name,bytes=item.size,"spool limit exceeded; dropping oldest unuploaded chunk");
            self.remove_unlocked(&item)?;
            bytes = bytes.saturating_sub(item.size);
        }
        Ok(path)
    }
    /// Snapshot the immutable source handle under the lock; never block collection on SMB.
    pub fn upload_item(&self, item: &SpoolItem, destination: &Path) -> Result<Option<u64>> {
        let (source, key, bytes) = {
            let _lock = self.lock.lock().unwrap();
            let source = match File::open(&item.path) {
                Ok(f) => f,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(e.into()),
            };
            let key = crate::win::file_key(&source)?;
            let bytes = source.metadata()?.len();
            (source, key, bytes)
        };
        if let Some(p) = destination.parent() {
            fs::create_dir_all(p)?;
        }
        let tmp = destination.with_file_name(format!(
            "{}.tmp",
            destination
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("missing destination filename"))?
                .to_string_lossy()
        ));
        let result = (|| -> Result<()> {
            let mut reader = source;
            let mut out = File::create(&tmp)?;
            io::copy(&mut reader, &mut out)?;
            out.sync_all()?;
            drop(out);
            crate::win::replace_file(&tmp, destination)?;
            Ok(())
        })();
        if let Err(error) = result {
            if let Err(cleanup) = fs::remove_file(&tmp) {
                if cleanup.kind() != io::ErrorKind::NotFound {
                    tracing::debug!(error=%cleanup,"could not clean remote temporary chunk");
                }
            }
            return Err(error);
        }
        let _lock = self.lock.lock().unwrap();
        let current = match File::open(&item.path) {
            Ok(f) => Some(f),
            Err(e) if e.kind() == io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        if let Some(current) = current {
            let same = crate::win::file_key(&current)? == key;
            drop(current);
            if same {
                self.remove_unlocked(item)?;
            }
        }
        Ok(Some(bytes))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn collisions_preserve_both_payloads_and_identical_retry_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let s = Spool::new(d.path()).unwrap();
        for body in [b"1\n".as_slice(), b"1\n", b"2\n", b"2\n"] {
            s.write_chunk(
                ChunkSpec {
                    name_seg: "same.csv",
                    start: 0,
                    end: 2,
                    kind: HeaderKind::Syn,
                    header: None,
                    body,
                },
                10000,
            )
            .unwrap();
        }
        assert_eq!(s.stats().unwrap().pending, 2);
    }
    #[test]
    fn upload_is_durable_and_failed_rename_cleans_tmp() {
        let d = tempfile::tempdir().unwrap();
        let s = Spool::new(d.path().join("spool")).unwrap();
        s.write_chunk(
            ChunkSpec {
                name_seg: "a",
                start: 0,
                end: 2,
                kind: HeaderKind::Syn,
                header: None,
                body: b"1\n",
            },
            1000,
        )
        .unwrap();
        let item = s.list_oldest_first().unwrap().remove(0);
        let dest = d.path().join("remote").join(&item.name);
        fs::create_dir_all(&dest).unwrap();
        assert!(s.upload_item(&item, &dest).is_err());
        assert!(item.path.exists());
        assert!(!dest.with_file_name(format!("{}.tmp", item.name)).exists());
        fs::remove_dir(&dest).unwrap();
        assert_eq!(s.upload_item(&item, &dest).unwrap(), Some(item.size));
        assert!(dest.is_file());
        assert!(!item.path.exists());
    }
    #[test]
    fn durable_gzip_guard_and_cleanup() {
        let d = tempfile::tempdir().unwrap();
        let spool = Spool::new(d.path()).unwrap();
        let p = spool
            .write_chunk(
                ChunkSpec {
                    name_seg: "a.csv",
                    start: 0,
                    end: 2,
                    kind: HeaderKind::Syn,
                    header: Some(b"c0"),
                    body: b"1\n",
                },
                1000,
            )
            .unwrap();
        let mut decoded = vec![];
        use std::io::Read;
        flate2::read::GzDecoder::new(File::open(&p).unwrap())
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, b"c0\n1\n");
        spool
            .write_chunk(
                ChunkSpec {
                    name_seg: "b.csv",
                    start: 0,
                    end: 2,
                    kind: HeaderKind::Syn,
                    header: None,
                    body: b"2\n",
                },
                0,
            )
            .unwrap();
        assert_eq!(spool.stats().unwrap().pending, 0);
    }
}
