pub use crate::config::HeaderKind;
use crate::{
    config::{Config, HeaderMode},
    paths::{normalize, CompiledPattern},
    spool::{ChunkSpec, Spool},
    state::{ProgressRow, StateDb},
};
use anyhow::{Context, Result};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    NoChange,
    NoNewline,
    Chunked { chunks: u32, bytes: u64 },
    Skipped(String),
}
pub fn process_file(db: &StateDb, spool: &Spool, config: &Config, path: &Path) -> Result<Outcome> {
    let Some(pattern) = config
        .watch
        .iter()
        .filter(|w| w.enabled)
        .filter_map(|w| CompiledPattern::new(w).ok())
        .find(|p| p.matches(path))
    else {
        return Ok(Outcome::Skipped("no enabled matching pattern".into()));
    };
    let mut file = File::open(path).with_context(|| format!("open CSV {}", path.display()))?;
    let key = crate::win::file_key(&file)?;
    let size = file.metadata()?.len();
    let now = chrono::Local::now().to_rfc3339();
    let norm = normalize(path).0;
    let mut row = if let Some(row) = db.get_progress(&key)? {
        row
    } else {
        let name = db.allocate_name(&pattern.name_segment(path), &key, &norm)?;
        ProgressRow {
            file_key: key,
            path: norm.clone(),
            display_path: path.to_string_lossy().into(),
            name_seg: name,
            offset: 0,
            size: 0,
            header_mode: pattern.cfg.header,
            header: None,
            header_kind: None,
            field_count: None,
            last_seen: now.clone(),
            updated_at: now.clone(),
            last_error: None,
        }
    };
    let fresh = pattern.name_segment(path);
    if let Some(upgraded) = crate::paths::case_upgraded(&row.name_seg, &fresh) {
        tracing::info!(old=%row.name_seg,new=%upgraded,"chunk name now keeps the source case");
        db.rename_source(&row.file_key, &norm, &upgraded, &fresh)?;
        row.name_seg = upgraded;
    }
    row.path = norm;
    row.display_path = path.to_string_lossy().into();
    row.last_seen = now;
    row.last_error = None;
    if size < row.offset || size < row.size {
        tracing::warn!(path=%path.display(),old_offset=row.offset,size,"CSV truncated; restarting from byte zero");
        row.name_seg = db.rotate_name(&row)?;
        row.offset = 0;
        row.header = None;
        row.header_kind = None;
        row.field_count = None;
    }
    if row.header_mode != pattern.cfg.header {
        row.header = None;
        row.header_kind = None;
        row.field_count = None;
        row.header_mode = pattern.cfg.header;
    }
    row.size = size;
    if size == row.offset {
        db.upsert_progress(&row)?;
        return Ok(Outcome::NoChange);
    }
    let mut header_len = 0;
    if row.header_mode == HeaderMode::Auto {
        let Some(line) = read_line_bounded(&mut file, 0, size, 64 << 10)? else {
            db.upsert_progress(&row)?;
            return Ok(Outcome::NoNewline);
        };
        header_len = line.len() as u64;
        let header = strip_newline(&line).to_vec();
        if row.header.as_ref().is_some_and(|old| old != &header) {
            tracing::warn!(path=%path.display(),"CSV source header changed");
        }
        row.header = Some(header);
        row.header_kind = Some(HeaderKind::Hdr);
    } else if row.header_mode == HeaderMode::Fixed {
        let columns = pattern
            .cfg
            .columns
            .as_ref()
            .filter(|c| !c.is_empty())
            .context("fixed header requires columns")?;
        row.header = Some(columns.join(pattern.cfg.delimiter()).into_bytes());
        row.header_kind = Some(HeaderKind::Syn);
    }
    db.upsert_progress(&row)?;
    let cap = usize::try_from(config.agent.max_chunk_bytes)
        .context("max chunk bytes exceeds address space")?;
    anyhow::ensure!(cap > 0, "max_chunk_bytes must be positive");
    let mut chunks = 0;
    let mut bytes = 0;
    while row.offset < size {
        let len = (size - row.offset).min(cap as u64) as usize;
        file.seek(SeekFrom::Start(row.offset))?;
        let mut buf = vec![0; len];
        file.read_exact(&mut buf)?;
        let Some(cut) = buf.iter().rposition(|b| *b == b'\n') else {
            if len == cap {
                tracing::warn!(path=%path.display(),offset=row.offset,max_chunk_bytes=cap,"CSV line exceeds chunk window; increase max_chunk_bytes to process");
            }
            break;
        };
        if row.header_mode == HeaderMode::Auto && row.offset == 0 && (cut as u64) < header_len {
            break;
        }
        if row.header_mode == HeaderMode::None {
            let first = buf.iter().position(|b| *b == b'\n').unwrap();
            let fields = count_fields(
                strip_newline(&buf[..=first]),
                pattern.cfg.delimiter().as_bytes(),
            );
            if row.field_count.is_some_and(|old| old != fields) {
                tracing::warn!(path=%path.display(),old=?row.field_count,new=fields,"CSV field count changed");
            }
            row.field_count = Some(fields);
            row.header = Some(
                (0..fields)
                    .map(|i| format!("c{i}"))
                    .collect::<Vec<_>>()
                    .join(pattern.cfg.delimiter())
                    .into_bytes(),
            );
            row.header_kind = Some(HeaderKind::Syn);
        }
        let end = row.offset + cut as u64 + 1;
        let header = if row.header_mode == HeaderMode::Auto && row.offset == 0 {
            None
        } else {
            row.header.as_deref()
        };
        spool.write_chunk(
            ChunkSpec {
                name_seg: &row.name_seg,
                start: row.offset,
                end,
                kind: row.header_kind.context("missing header kind")?,
                header,
                body: &buf[..=cut],
            },
            config.agent.spool_max_bytes,
        )?;
        let start = row.offset;
        row.offset = end;
        row.updated_at = chrono::Local::now().to_rfc3339();
        db.commit_offset(&row)?;
        chunks += 1;
        bytes += end - start;
        tracing::info!(path=%path.display(),start,end,kind=?row.header_kind,"CSV chunk committed");
    }
    if chunks > 0 {
        Ok(Outcome::Chunked { chunks, bytes })
    } else {
        Ok(Outcome::NoNewline)
    }
}
fn strip_newline(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}
fn read_line_bounded(
    file: &mut File,
    start: u64,
    size: u64,
    cap: usize,
) -> Result<Option<Vec<u8>>> {
    file.seek(SeekFrom::Start(start))?;
    let mut buf = vec![0; ((size - start).min(cap as u64)) as usize];
    file.read_exact(&mut buf)?;
    Ok(buf.iter().position(|b| *b == b'\n').map(|n| {
        buf.truncate(n + 1);
        buf
    }))
}
pub fn count_fields(line: &[u8], delimiter: &[u8]) -> u32 {
    if delimiter.is_empty() {
        return 1;
    }
    let mut fields = 1;
    let mut quoted = false;
    let mut i = 0;
    while i < line.len() {
        if line[i] == b'"' {
            if quoted && line.get(i + 1) == Some(&b'"') {
                i += 2;
                continue;
            }
            quoted = !quoted;
            i += 1;
        } else if !quoted && line[i..].starts_with(delimiter) {
            fields += 1;
            i += delimiter.len();
        } else {
            i += 1;
        }
    }
    fields
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    fn setup(
        mode: HeaderMode,
    ) -> (
        tempfile::TempDir,
        Config,
        StateDb,
        Spool,
        std::path::PathBuf,
    ) {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("data.csv");
        let config = Config {
            watch: vec![crate::config::WatchConfig {
                pattern: p.to_string_lossy().into(),
                header: mode,
                ..Default::default()
            }],
            ..Default::default()
        };
        let db = StateDb::open(&d.path().join("state.db")).unwrap();
        let spool = Spool::new(d.path().join("spool")).unwrap();
        (d, config, db, spool, p)
    }
    fn decoded(spool: &Spool) -> Vec<Vec<u8>> {
        spool
            .list_oldest_first()
            .unwrap()
            .iter()
            .map(|i| {
                let mut v = vec![];
                flate2::read::GzDecoder::new(File::open(&i.path).unwrap())
                    .read_to_end(&mut v)
                    .unwrap();
                v
            })
            .collect()
    }
    #[test]
    fn auto_preserves_bytes_crlf_and_partial_append() {
        let (_d, c, db, s, p) = setup(HeaderMode::Auto);
        std::fs::write(&p, b"\xff,name\r\n1,x\r\n2,").unwrap();
        assert!(matches!(
            process_file(&db, &s, &c, &p).unwrap(),
            Outcome::Chunked { chunks: 1, .. }
        ));
        assert_eq!(decoded(&s)[0], b"\xff,name\r\n1,x\r\n");
        assert_eq!(process_file(&db, &s, &c, &p).unwrap(), Outcome::NoNewline);
        File::options()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"y\r\n")
            .unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        assert_eq!(decoded(&s)[1], b"\xff,name\n2,y\r\n");
    }
    #[test]
    fn auto_header_only_waits() {
        let (_d, c, db, s, p) = setup(HeaderMode::Auto);
        std::fs::write(&p, b"a,b\n").unwrap();
        assert_eq!(process_file(&db, &s, &c, &p).unwrap(), Outcome::NoNewline);
        assert_eq!(db.load_all_progress().unwrap()[0].offset, 0);
        assert_eq!(s.stats().unwrap().pending, 0);
    }
    #[test]
    fn none_first_row_is_data_and_quote_delimiters() {
        let (_d, c, db, s, p) = setup(HeaderMode::None);
        std::fs::write(&p, b"1,\"a,b\",3\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        assert_eq!(decoded(&s)[0], b"c0,c1,c2\n1,\"a,b\",3\n");
        assert_eq!(count_fields(b"\"a\"\"b,c\",d", b","), 2);
    }
    #[test]
    fn spool_failure_does_not_advance_offset() {
        let (_d, c, db, s, p) = setup(HeaderMode::None);
        std::fs::write(&p, b"1,2\n").unwrap();
        std::fs::remove_dir(&s.dir).unwrap();
        std::fs::write(&s.dir, b"obstruction").unwrap();
        assert!(process_file(&db, &s, &c, &p).is_err());
        assert_eq!(db.load_all_progress().unwrap()[0].offset, 0);
    }
    #[test]
    fn truncate_resets_and_fixed_always_inserts_header() {
        let (_d, mut c, db, s, p) = setup(HeaderMode::Fixed);
        c.watch[0].columns = Some(vec!["x".into()]);
        std::fs::write(&p, b"12345\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        std::fs::write(&p, b"2\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        assert!(decoded(&s).iter().any(|v| v == b"x\n2\n"));
        assert_eq!(db.load_all_progress().unwrap()[0].offset, 2);
    }
    #[test]
    fn observed_truncate_preserves_original_offset_range() {
        let (_d, c, db, s, p) = setup(HeaderMode::None);
        std::fs::write(&p, b"1\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        let first = db.load_all_progress().unwrap()[0].name_seg.clone();
        std::fs::write(&p, b"").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        std::fs::write(&p, b"2\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        assert_ne!(db.load_all_progress().unwrap()[0].name_seg, first);
        let chunks = decoded(&s);
        assert_eq!(chunks.len(), 2);
        assert!(chunks.iter().any(|v| v == b"c0\n1\n"));
        assert!(chunks.iter().any(|v| v == b"c0\n2\n"));
    }
    #[test]
    fn replacement_keeps_offline_generations() {
        let (_d, c, db, s, p) = setup(HeaderMode::None);
        std::fs::write(&p, b"1\n").unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        db.delete_progress_by_path(&normalize(&p).0).unwrap();
        let replacement = p.with_extension("new");
        std::fs::write(&replacement, b"2\n").unwrap();
        crate::win::replace_file(&replacement, &p).unwrap();
        process_file(&db, &s, &c, &p).unwrap();
        assert_eq!(s.stats().unwrap().pending, 2);
    }
    #[test]
    fn legacy_lowercase_names_upgrade_to_source_case() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("Data_A.CSV");
        let config = Config {
            watch: vec![crate::config::WatchConfig {
                pattern: p.to_string_lossy().into(),
                header: HeaderMode::None,
                ..Default::default()
            }],
            ..Default::default()
        };
        let db = StateDb::open(&d.path().join("state.db")).unwrap();
        let s = Spool::new(d.path().join("spool")).unwrap();
        std::fs::write(&p, b"1\n").unwrap();
        let key = crate::win::file_key(&File::open(&p).unwrap()).unwrap();
        // What a build before the case fix stored for this file.
        db.allocate_name("data_a.csv", &key, &normalize(&p).0)
            .unwrap();
        process_file(&db, &s, &config, &p).unwrap();
        File::options()
            .append(true)
            .open(&p)
            .unwrap()
            .write_all(b"2\n")
            .unwrap();
        process_file(&db, &s, &config, &p).unwrap();
        db.reset_offset(&key).unwrap();
        process_file(&db, &s, &config, &p).unwrap();
        let names: Vec<String> = s
            .list_oldest_first()
            .unwrap()
            .into_iter()
            .map(|i| i.name)
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(
            names.iter().all(|n| n.starts_with("Data_A.CSV__")),
            "{names:?}"
        );
    }
    #[test]
    fn chunks_split_at_newlines_and_keep_partial_tail() {
        let (_d, mut c, db, s, p) = setup(HeaderMode::None);
        c.agent.max_chunk_bytes = 5;
        std::fs::write(&p, b"1\n22\n333\n4").unwrap();
        let out = process_file(&db, &s, &c, &p).unwrap();
        assert_eq!(
            out,
            Outcome::Chunked {
                chunks: 2,
                bytes: 9
            }
        );
        assert_eq!(db.load_all_progress().unwrap()[0].offset, 9);
        assert_eq!(
            decoded(&s),
            vec![b"c0\n1\n22\n".to_vec(), b"c0\n333\n".to_vec()]
        );
    }
}
