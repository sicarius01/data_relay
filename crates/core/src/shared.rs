use crate::{bus::*, config::Config, state::ProgressRow};
use chrono::{DateTime, Local};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::watch;

pub struct SharedState {
    inner: Mutex<StatusSnapshot>,
    status_tx: watch::Sender<Arc<StatusSnapshot>>,
    on_change: Arc<dyn Fn() + Send + Sync>,
    pub started_at: DateTime<Local>,
    pub equip_id_changed_from: Mutex<Option<String>>,
    pub counter_date: Mutex<String>,
    pub lifetime_guards: Mutex<Vec<crate::win::InstanceLock>>,
    spool_refresh_lock: Mutex<()>,
    error_log_times: Mutex<HashMap<(String, String), Instant>>,
}
impl SharedState {
    pub fn new(
        config: &Config,
        on_change: Arc<dyn Fn() + Send + Sync>,
    ) -> (Arc<Self>, watch::Receiver<Arc<StatusSnapshot>>) {
        let snapshot = StatusSnapshot {
            generated_at: Local::now(),
            equip_id: config.equip_id().ok(),
            tray: TrayState::Ok,
            config_errors: config
                .validate()
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect(),
            files: vec![],
            watch_errors: vec![],
            spool_pending: 0,
            spool_bytes: 0,
            bytes_sent_today: 0,
            last_upload_at: None,
            last_error: None,
            uploader: UploaderState::Gated,
            tasks: vec![],
        };
        let (status_tx, status_rx) = watch::channel(Arc::new(snapshot.clone()));
        let shared = Arc::new(Self {
            inner: Mutex::new(snapshot),
            status_tx,
            on_change,
            started_at: Local::now(),
            equip_id_changed_from: Mutex::new(None),
            counter_date: Mutex::new(Local::now().format("%Y%m%d").to_string()),
            lifetime_guards: Mutex::new(vec![]),
            spool_refresh_lock: Mutex::new(()),
            error_log_times: Mutex::new(HashMap::new()),
        });
        shared.update(|_| {});
        (shared, status_rx)
    }
    pub fn snapshot(&self) -> Arc<StatusSnapshot> {
        self.status_tx.borrow().clone()
    }
    pub fn repaint(&self) {
        (self.on_change)();
    }
    pub fn refresh_spool(&self, spool: &crate::spool::Spool) -> anyhow::Result<u64> {
        let _guard = self.spool_refresh_lock.lock().unwrap();
        let stats = spool.stats()?;
        self.update(|s| {
            s.spool_pending = stats.pending;
            s.spool_bytes = stats.bytes;
        });
        Ok(stats.pending)
    }
    pub fn update(&self, f: impl FnOnce(&mut StatusSnapshot)) {
        {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            f(&mut inner);
            inner.generated_at = Local::now();
            inner.tray = if !inner.config_errors.is_empty()
                || !inner.watch_errors.is_empty()
                || inner.uploader == UploaderState::Suspended
            {
                TrayState::Error
            } else if inner.spool_pending > 0 {
                TrayState::Pending
            } else {
                TrayState::Ok
            };
            self.status_tx.send_replace(Arc::new(inner.clone()));
        }
        self.repaint();
    }
    pub fn error(&self, component: &str, message: impl Into<String>) {
        let message = message.into();
        {
            let now = Instant::now();
            let mut times = self.error_log_times.lock().unwrap();
            times.retain(|_, last| now.duration_since(*last) < Duration::from_secs(300));
            if !times.contains_key(&(component.into(), message.clone())) {
                tracing::warn!(component, %message, "worker error");
                if times.len() >= 256 {
                    times.clear();
                }
                times.insert((component.into(), message.clone()), now);
            }
        }
        self.update(|s| {
            s.last_error = Some(ErrorInfo {
                at: Local::now(),
                component: component.into(),
                message,
            })
        });
    }
    pub fn apply_config(&self, config: &Config) {
        let equip = config.equip_id().ok();
        let previous = self.snapshot().equip_id.clone();
        if equip != previous {
            tracing::warn!(?previous, ?equip, "equipment identifier changed");
            *self.equip_id_changed_from.lock().unwrap() = previous;
        }
        self.update(|s| {
            s.equip_id = equip;
            s.config_errors = config
                .validate()
                .iter()
                .map(|e| format!("{}: {}", e.field, e.message))
                .collect();
        });
    }
    pub fn refresh_progress(&self, rows: Vec<ProgressRow>) {
        self.update(|s| {
            s.files = rows
                .into_iter()
                .map(|row| {
                    let present = s
                        .files
                        .iter()
                        .find(|f| f.file_key == row.file_key)
                        .map(|f| f.present)
                        .unwrap_or(false);
                    FileStatus {
                        file_key: row.file_key,
                        path: row.display_path.into(),
                        size: row.size,
                        offset: row.offset,
                        header_kind: row.header_kind,
                        header_mode: row.header_mode,
                        last_processed: DateTime::parse_from_rfc3339(&row.updated_at)
                            .ok()
                            .map(|t| t.with_timezone(&Local)),
                        present,
                        last_error: row.last_error,
                    }
                })
                .collect();
            s.files.sort_by(|a, b| a.path.cmp(&b.path));
        });
    }
}
