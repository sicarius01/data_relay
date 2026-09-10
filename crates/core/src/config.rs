use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, fs, io::Write, path::Path};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderMode {
    #[default]
    Auto,
    None,
    Fixed,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeaderKind {
    Hdr,
    Syn,
}
impl HeaderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hdr => "hdr",
            Self::Syn => "syn",
        }
    }
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    #[default]
    Hourly,
    EveryNMin,
    Daily,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub equip: EquipConfig,
    pub agent: AgentConfig,
    pub server: ServerConfig,
    pub ui: UiConfig,
    pub watch: Vec<WatchConfig>,
    pub task: Vec<TaskConfig>,
    pub log: LogConfig,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EquipConfig {
    pub num: String,
    pub name: String,
    pub id_separator: String,
}
impl Default for EquipConfig {
    fn default() -> Self {
        Self {
            num: String::new(),
            name: String::new(),
            id_separator: "_".into(),
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub debounce_sec: u64,
    pub max_wait_sec: u64,
    pub spool_dir: String,
    pub spool_max_bytes: u64,
    pub heartbeat_interval_sec: u64,
    pub autostart: bool,
    pub max_chunk_bytes: u64,
}
impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            debounce_sec: 3,
            max_wait_sec: 180,
            spool_dir: "C:/ProgramData/csv-agent/spool".into(),
            spool_max_bytes: 1 << 30,
            heartbeat_interval_sec: 300,
            autostart: true,
            max_chunk_bytes: 8 << 20,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    pub unc_base: String,
    pub retry_max: u32,
    pub retry_base_ms: u64,
}
impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            unc_base: String::new(),
            retry_max: 10,
            retry_base_ms: 1000,
        }
    }
}
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    pub exit_confirm_message: String,
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WatchConfig {
    pub pattern: String,
    pub header: HeaderMode,
    pub columns: Option<Vec<String>>,
    pub delimiter: String,
    pub enabled: bool,
}
impl Default for WatchConfig {
    fn default() -> Self {
        Self {
            pattern: String::new(),
            header: HeaderMode::Auto,
            columns: None,
            delimiter: ",".into(),
            enabled: true,
        }
    }
}
impl WatchConfig {
    pub fn delimiter(&self) -> &str {
        if self.delimiter == "\\t" {
            "\t"
        } else {
            &self.delimiter
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskConfig {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub working_dir: Option<String>,
    pub schedule: ScheduleKind,
    pub interval_min: Option<u32>,
    pub daily_at: Option<String>,
    pub timeout_sec: u64,
    pub run_on_start: bool,
    pub enabled: bool,
}
impl Default for TaskConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            program: String::new(),
            args: vec![],
            working_dir: None,
            schedule: ScheduleKind::Hourly,
            interval_min: None,
            daily_at: None,
            timeout_sec: 300,
            run_on_start: false,
            enabled: true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogConfig {
    pub level: String,
    pub dir: String,
    pub max_files: usize,
}
impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: "info".into(),
            dir: "C:/ProgramData/csv-agent/logs".into(),
            max_files: 14,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigError {
    pub field: String,
    pub message: String,
}
impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}
#[derive(Debug)]
// Loaded once at startup; keeping the owned config avoids allocation and an API indirection.
#[allow(clippy::large_enum_variant)]
pub enum ConfigLoad {
    Loaded {
        config: Config,
        errors: Vec<ConfigError>,
    },
    Broken {
        err: String,
    },
}

pub fn valid_unc(s: &str) -> bool {
    let Some(t) = s.strip_prefix("\\\\") else {
        return false;
    };
    let mut parts = t.split('\\');
    matches!((parts.next(),parts.next()),(Some(a),Some(b)) if !a.trim().is_empty() && !b.trim().is_empty() && !a.contains('/') && !b.contains('/') && a!="?" && a!="." && b!="." && b!="..")
        && !parts.any(|p| p == ".." || p == ".")
}
pub fn valid_daily(s: &str) -> bool {
    s.len() == 5
        && s.is_ascii()
        && s.as_bytes()[2] == b':'
        && s[..2].bytes().all(|b| b.is_ascii_digit())
        && s[3..].bytes().all(|b| b.is_ascii_digit())
        && s[..2].parse::<u32>().is_ok_and(|n| n < 24)
        && s[3..].parse::<u32>().is_ok_and(|n| n < 60)
}
impl Config {
    pub fn equip_id(&self) -> std::result::Result<String, Vec<ConfigError>> {
        let mut e = vec![];
        let mut check = |field: &str, ok: bool, msg: &str| {
            if !ok {
                e.push(ConfigError {
                    field: field.into(),
                    message: msg.into(),
                })
            }
        };
        check(
            "equip.num",
            !self.equip.num.trim().is_empty(),
            "장비 번호가 필요합니다",
        );
        check(
            "equip.name",
            !self.equip.name.trim().is_empty(),
            "장비 이름이 필요합니다",
        );
        check(
            "equip.id_separator",
            !self.equip.id_separator.is_empty()
                && !self
                    .equip
                    .id_separator
                    .chars()
                    .any(crate::paths::invalid_filename_char),
            "유효한 구분자가 필요합니다",
        );
        let id = format!(
            "{}{}{}",
            crate::paths::sanitize(&self.equip.num),
            self.equip.id_separator,
            crate::paths::sanitize(&self.equip.name)
        );
        check(
            "equip_id",
            id.chars().count() <= 64,
            "장비 식별자는 최대 64자입니다",
        );
        check(
            "equip_id",
            !id.ends_with('.')
                && !id.ends_with(' ')
                && id != "."
                && id != ".."
                && !crate::paths::is_reserved_name(&id),
            "Windows 경로로 사용할 수 없는 식별자입니다",
        );
        if e.is_empty() {
            Ok(id)
        } else {
            Err(e)
        }
    }
    pub fn validate(&self) -> Vec<ConfigError> {
        let mut e = self.equip_id().err().unwrap_or_default();
        let mut check = |field: String, ok: bool, msg: &str| {
            if !ok {
                e.push(ConfigError {
                    field,
                    message: msg.into(),
                })
            }
        };
        check(
            "server.unc_base".into(),
            valid_unc(&self.server.unc_base),
            "UNC 경로는 \\\\host\\share 형식이어야 합니다",
        );
        check(
            "agent.debounce_sec".into(),
            self.agent.debounce_sec >= 1,
            "1초 이상이어야 합니다",
        );
        check(
            "agent.max_wait_sec".into(),
            self.agent.max_wait_sec >= self.agent.debounce_sec,
            "debounce_sec 이상이어야 합니다",
        );
        check(
            "agent.spool_max_bytes".into(),
            self.agent.spool_max_bytes >= 16 << 20,
            "16 MiB 이상이어야 합니다",
        );
        check(
            "agent.max_chunk_bytes".into(),
            self.agent.max_chunk_bytes >= 64 << 10,
            "64 KiB 이상이어야 합니다",
        );
        check(
            "agent.heartbeat_interval_sec".into(),
            self.agent.heartbeat_interval_sec > 0,
            "1초 이상이어야 합니다",
        );
        check(
            "server.retry_base_ms".into(),
            self.server.retry_base_ms > 0,
            "1 ms 이상이어야 합니다",
        );
        for (i, w) in self.watch.iter().enumerate() {
            check(
                format!("watch[{i}].pattern"),
                crate::paths::CompiledPattern::new(w).is_ok(),
                "절대경로 glob 패턴이 필요합니다",
            );
            check(
                format!("watch[{i}].delimiter"),
                w.delimiter().chars().count() == 1 && !w.delimiter().contains(['\r', '\n', '"']),
                "개행/따옴표 이외의 한 문자 구분자가 필요합니다",
            );
            if w.header == HeaderMode::Fixed {
                check(
                    format!("watch[{i}].columns"),
                    w.columns.as_ref().is_some_and(|v| {
                        !v.is_empty()
                            && v.iter().all(|c| {
                                !c.trim().is_empty()
                                    && !c.contains(w.delimiter())
                                    && !c.contains(['\r', '\n'])
                            })
                    }),
                    "비어 있지 않고 구분자를 포함하지 않는 컬럼명이 필요합니다",
                );
            }
        }
        let mut names = HashSet::new();
        for (i, t) in self.task.iter().enumerate() {
            check(
                format!("task[{i}].name"),
                !t.name.trim().is_empty() && names.insert(&t.name),
                "비어 있지 않은 고유 이름이 필요합니다",
            );
            check(
                format!("task[{i}].program"),
                !t.program.trim().is_empty(),
                "프로그램이 필요합니다",
            );
            check(
                format!("task[{i}].timeout_sec"),
                t.timeout_sec > 0,
                "1초 이상이어야 합니다",
            );
            match t.schedule {
                ScheduleKind::EveryNMin => check(
                    format!("task[{i}].interval_min"),
                    t.interval_min.is_some_and(|n| (1..=1440).contains(&n)),
                    "1..1440분이어야 합니다",
                ),
                ScheduleKind::Daily => check(
                    format!("task[{i}].daily_at"),
                    t.daily_at.as_ref().is_some_and(|s| valid_daily(s)),
                    "HH:MM 형식이어야 합니다",
                ),
                _ => {}
            }
        }
        check(
            "log.level".into(),
            ["trace", "debug", "info", "warn", "error"].contains(&self.log.level.as_str()),
            "유효하지 않은 로그 레벨입니다",
        );
        check(
            "log.max_files".into(),
            self.log.max_files > 0,
            "1 이상이어야 합니다",
        );
        e
    }
}
pub fn load(path: &Path) -> Result<ConfigLoad> {
    let text = match fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let c = Config::default();
            save(path, &c)?;
            return Ok(ConfigLoad::Loaded {
                errors: c.validate(),
                config: c,
            });
        }
        Err(e) => return Err(e.into()),
    };
    Ok(match toml::from_str::<Config>(&text) {
        Ok(config) => ConfigLoad::Loaded {
            errors: config.validate(),
            config,
        },
        Err(e) => ConfigLoad::Broken { err: e.to_string() },
    })
}
pub fn save(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let data = toml::to_string_pretty(config)?;
    let tmp = path.with_extension("toml.tmp");
    let mut f = fs::File::create(&tmp)?;
    f.write_all(data.as_bytes())?;
    f.sync_all()?;
    drop(f);
    if path.exists() {
        fs::copy(path, path.with_extension("toml.bak"))?;
    }
    crate::win::replace_file(&tmp, path)?;
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn defaults_and_validation() {
        let c: Config = toml::from_str("").unwrap();
        assert_eq!(c.agent.max_wait_sec, 180);
        assert!(!c.validate().is_empty());
        assert!(valid_unc(r"\\host\share"));
        assert!(!valid_unc(r"\\host"));
        assert!(!valid_daily("25:00"));
    }
    #[test]
    fn broken_config_is_preserved() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("config.toml");
        fs::write(&p, "[broken").unwrap();
        assert!(matches!(load(&p).unwrap(), ConfigLoad::Broken { .. }));
        assert_eq!(fs::read_to_string(&p).unwrap(), "[broken");
        save(&p, &Config::default()).unwrap();
        assert_eq!(
            fs::read_to_string(p.with_extension("toml.bak")).unwrap(),
            "[broken"
        );
    }
}
