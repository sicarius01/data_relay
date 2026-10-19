use crate::config::{HeaderKind, WatchConfig};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
#[derive(Debug, Clone)]
pub struct AppPaths {
    pub base: PathBuf,
    pub config: PathBuf,
    pub state: PathBuf,
    pub spool: PathBuf,
    pub logs: PathBuf,
}
impl AppPaths {
    pub fn resolve() -> Result<Self> {
        let base = std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/ProgramData"))
            .join("csv-agent");
        std::fs::create_dir_all(&base)?;
        Ok(Self::from_base(base))
    }
    pub fn from_base(base: impl Into<PathBuf>) -> Self {
        let base = base.into();
        Self {
            config: base.join("config.toml"),
            state: base.join("state.db"),
            spool: base.join("spool"),
            logs: base.join("logs"),
            base,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct NormPath(pub String);
impl AsRef<str> for NormPath {
    fn as_ref(&self) -> &str {
        &self.0
    }
}
impl std::fmt::Display for NormPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
/// Separator and prefix cleanup only; keeps the original case for display and chunk names.
fn cleaned_text(s: &str) -> String {
    let s = s.replace('\\', "/");
    let s = if let Some(s) = s.strip_prefix("//?/UNC/") {
        format!("//{s}")
    } else {
        s.strip_prefix("//?/").unwrap_or(&s).to_owned()
    };
    if s.len() > 3 {
        s.trim_end_matches('/').to_owned()
    } else {
        s
    }
}
fn normalized_text(s: &str) -> String {
    cleaned_text(s).to_lowercase()
}
fn absolute_text(p: &Path) -> String {
    let s = p.to_string_lossy();
    let absolute =
        s.starts_with('/') || s.starts_with('\\') || (s.as_bytes().get(1) == Some(&b':'));
    if absolute {
        s.into_owned()
    } else {
        std::path::absolute(p)
            .unwrap_or_else(|_| p.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }
}
pub fn normalize(p: &Path) -> NormPath {
    NormPath(normalized_text(&absolute_text(p)))
}
pub fn invalid_filename_char(c: char) -> bool {
    c.is_control() || "\\/:*?\"<>|".contains(c)
}
pub fn sanitize(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| {
            if invalid_filename_char(c) || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .collect()
}
pub fn is_reserved_name(s: &str) -> bool {
    let stem = s.split('.').next().unwrap_or("").to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && matches!(stem.as_bytes()[3], b'1'..=b'9'))
}
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub source: String,
    pub prefix: PathBuf,
    pub glob: glob::Pattern,
    pub opts: glob::MatchOptions,
    pub cfg: WatchConfig,
}
impl CompiledPattern {
    pub fn new(cfg: &WatchConfig) -> Result<Self> {
        let source = cfg.pattern.replace('\\', "/");
        let bytes = source.as_bytes();
        if !(source.starts_with('/')
            || (bytes.len() >= 3
                && bytes[0].is_ascii_alphabetic()
                && bytes[1] == b':'
                && bytes[2] == b'/'))
        {
            bail!("pattern must be an absolute path")
        }
        let glob = glob::Pattern::new(&normalized_text(&source))?;
        let first_meta = source.find(['*', '?', '[', '{']).unwrap_or(source.len());
        let prefix_end = source[..first_meta]
            .rfind('/')
            .ok_or_else(|| anyhow::anyhow!("pattern has no fixed directory"))?;
        let prefix = if prefix_end == 0 {
            "/"
        } else if prefix_end == 2 && bytes[1] == b':' {
            &source[..3]
        } else {
            &source[..prefix_end]
        };
        if prefix.is_empty() || prefix == "/" && source.starts_with("//") {
            bail!("missing fixed prefix")
        }
        if source.starts_with("//") && prefix.trim_start_matches('/').split('/').count() < 2 {
            bail!("UNC pattern requires fixed host and share")
        }
        Ok(Self {
            source: cfg.pattern.clone(),
            prefix: PathBuf::from(prefix),
            glob,
            opts: glob::MatchOptions {
                case_sensitive: false,
                require_literal_separator: true,
                require_literal_leading_dot: false,
            },
            cfg: cfg.clone(),
        })
    }
    pub fn matches(&self, path: &Path) -> bool {
        self.glob.matches_with(&normalize(path).0, self.opts)
    }
    pub fn name_segment(&self, path: &Path) -> String {
        let p = normalize(path).0;
        // Match the prefix case-insensitively, but keep the source file's own case in the name.
        let shown = cleaned_text(&absolute_text(path));
        let root = normalize(&self.prefix).0;
        let root_parts: Vec<&str> = root.trim_end_matches('/').split('/').collect();
        let parts: Vec<&str> = shown.split('/').collect();
        let under_root = parts.len() > root_parts.len()
            && root_parts
                .iter()
                .zip(&parts)
                .all(|(r, s)| s.to_lowercase() == *r);
        let rel = if under_root {
            parts[root_parts.len()..].join("/")
        } else {
            shown.clone()
        };
        // Escape literal '~' to keep nested paths distinguishable from filenames containing '~'.
        let mut name = rel
            .replace('%', "%25")
            .replace('~', "%7e")
            .replace('/', "~")
            .chars()
            .map(|c| if invalid_filename_char(c) { '_' } else { c })
            .collect::<String>();
        if name.len() > 150 {
            let h = stable_hash(p.as_bytes());
            while name.len() > 120 {
                name.pop();
            }
            name.push_str(&format!("~{h:016x}"));
        }
        if is_reserved_name(&name) {
            name.insert(0, '_');
        }
        name
    }
}
/// Builds before the case fix stored lowercased names. Returns the stored name with its
/// lowercased base replaced by the source-case base, keeping any `~g…`/`~c…` suffix.
pub fn case_upgraded(stored: &str, fresh: &str) -> Option<String> {
    let lower = fresh.to_lowercase();
    if stored != stored.to_lowercase() || lower == fresh {
        return None;
    }
    let rest = stored.strip_prefix(&lower)?;
    (rest.is_empty() || rest.starts_with('~')).then(|| format!("{fresh}{rest}"))
}
pub fn stable_hash(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325u64, |a, b| {
        (a ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    })
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkName {
    pub name_seg: String,
    pub start: u64,
    pub end: u64,
    pub kind: HeaderKind,
}
pub fn format_chunk_name(n: &ChunkName) -> String {
    format!(
        "{}__{}-{}__{}.csv.gz",
        n.name_seg,
        n.start,
        n.end,
        n.kind.as_str()
    )
}
pub fn parse_chunk_name(s: &str) -> Option<ChunkName> {
    let s = s.strip_suffix(".csv.gz")?;
    let (name_range, kind) = s.rsplit_once("__")?;
    let (name, range) = name_range.rsplit_once("__")?;
    let (a, b) = range.split_once('-')?;
    let start = a.parse().ok()?;
    let end = b.parse().ok()?;
    if name.is_empty() || name.contains(['\\', '/']) || start >= end {
        return None;
    }
    Some(ChunkName {
        name_seg: name.into(),
        start,
        end,
        kind: match kind {
            "hdr" => HeaderKind::Hdr,
            "syn" => HeaderKind::Syn,
            _ => return None,
        },
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patterns_and_names() {
        let p = CompiledPattern::new(&WatchConfig {
            pattern: "D:/logs/**/vib_*.csv".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(p.prefix, PathBuf::from("D:/logs"));
        assert!(p.matches(Path::new("d:/LOGS/a/vib_1.csv")));
        let n = ChunkName {
            name_seg: "a__b.csv".into(),
            start: 0,
            end: 10,
            kind: HeaderKind::Hdr,
        };
        assert_eq!(parse_chunk_name(&format_chunk_name(&n)), Some(n));
        assert_ne!(
            p.name_segment(Path::new("D:/logs/a/b.csv")),
            p.name_segment(Path::new("D:/logs/a~b.csv"))
        );
        assert_ne!(
            p.name_segment(Path::new("D:/logs/a~b.csv")),
            p.name_segment(Path::new("D:/logs/a%7eb.csv"))
        );
    }
    #[test]
    fn name_segment_keeps_original_case() {
        let p = CompiledPattern::new(&WatchConfig {
            pattern: "D:/Logs/**/*.csv".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            p.name_segment(Path::new(r"d:\LOGS\LineA\Vib_001.CSV")),
            "LineA~Vib_001.CSV"
        );
        let unc = CompiledPattern::new(&WatchConfig {
            pattern: "//Server/Share/*.csv".into(),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            unc.name_segment(Path::new(r"\\?\UNC\SERVER\share\Data_A.csv")),
            "Data_A.csv"
        );
    }
    #[test]
    fn legacy_lowercase_names_are_upgraded() {
        assert_eq!(
            case_upgraded("linea~vib.csv", "LineA~Vib.CSV").as_deref(),
            Some("LineA~Vib.CSV")
        );
        assert_eq!(
            case_upgraded("vib.csv~g1-00000000000000ab", "Vib.CSV").as_deref(),
            Some("Vib.CSV~g1-00000000000000ab")
        );
        // Already in source case, all-lowercase sources, and unrelated names stay.
        assert_eq!(case_upgraded("Vib.CSV", "Vib.CSV"), None);
        assert_eq!(case_upgraded("vib.csv", "vib.csv"), None);
        assert_eq!(case_upgraded("other.csv", "Vib.CSV"), None);
        assert_eq!(case_upgraded("vib.csvx", "Vib.CSV"), None);
    }
    #[test]
    fn extended_unc() {
        assert_eq!(
            normalize(Path::new(r"\\?\UNC\SERVER\Share\A.CSV")).0,
            "//server/share/a.csv"
        );
    }
}
