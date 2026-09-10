use crate::{
    config::{Config, HeaderKind, HeaderMode, TaskConfig},
    state::TaskRun,
    win::FileKey,
};
use chrono::{DateTime, Local};
use std::path::PathBuf;
use tokio::sync::oneshot;

#[derive(Debug)]
#[allow(clippy::large_enum_variant)] // Infrequent GUI commands preserve the public Config-by-value API.
pub enum Command {
    ReloadConfig(Config),
    FlushNow,
    ProcessFileNow(PathBuf),
    ResetOffset(FileKey),
    RunTaskNow(String),
    TestTask {
        config: TaskConfig,
        reply: oneshot::Sender<Result<TaskRun, String>>,
    },
    TestUnc {
        base: String,
        reply: oneshot::Sender<Result<(), String>>,
    },
    PreviewPattern {
        pattern: String,
        reply: oneshot::Sender<Result<Vec<PathBuf>, String>>,
    },
    TaskHistory {
        name: String,
        reply: oneshot::Sender<Vec<TaskRun>>,
    },
    ConfigError(String),
    SystemResumed,
    Shutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayState {
    Ok,
    Pending,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploaderState {
    Idle,
    Draining,
    Backoff {
        until: tokio::time::Instant,
        attempt: u32,
    },
    Suspended,
    Gated,
}
impl UploaderState {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Draining => "draining",
            Self::Backoff { .. } => "backoff",
            Self::Suspended => "suspended",
            Self::Gated => "gated",
        }
    }
}
#[derive(Clone, Debug)]
pub struct ErrorInfo {
    pub at: DateTime<Local>,
    pub component: String,
    pub message: String,
}
#[derive(Clone, Debug, serde::Serialize)]
pub struct WatchError {
    pub pattern: String,
    pub reason: String,
}
#[derive(Clone, Debug)]
pub struct FileStatus {
    pub file_key: FileKey,
    pub path: PathBuf,
    pub size: u64,
    pub offset: u64,
    pub header_kind: Option<HeaderKind>,
    pub header_mode: HeaderMode,
    pub last_processed: Option<DateTime<Local>>,
    pub present: bool,
    pub last_error: Option<String>,
}
#[derive(Clone, Debug)]
pub struct TaskStatus {
    pub name: String,
    pub enabled: bool,
    pub next_run: Option<DateTime<Local>>,
    pub running: bool,
    pub last: Option<TaskRun>,
}
#[derive(Clone, Debug)]
pub struct StatusSnapshot {
    pub generated_at: DateTime<Local>,
    pub equip_id: Option<String>,
    pub tray: TrayState,
    pub config_errors: Vec<String>,
    pub files: Vec<FileStatus>,
    pub watch_errors: Vec<WatchError>,
    pub spool_pending: u64,
    pub spool_bytes: u64,
    pub bytes_sent_today: u64,
    pub last_upload_at: Option<DateTime<Local>>,
    pub last_error: Option<ErrorInfo>,
    pub uploader: UploaderState,
    pub tasks: Vec<TaskStatus>,
}

#[derive(Clone, Debug)]
pub enum FsEvent {
    Changed(PathBuf),
    Removed(PathBuf),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    Quiet,
    MaxWait,
    Manual,
}
#[derive(Clone, Debug)]
pub enum ProcessRequest {
    Process { path: PathBuf, reason: Reason },
    Removed(PathBuf),
    Reset(FileKey),
}
