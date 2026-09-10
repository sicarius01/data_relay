use crate::{
    bus::UploaderState, config::Config, shared::SharedState, spool::Spool, state::StateDb,
};
use anyhow::{Context, Result};
use chrono::Local;
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

pub fn backoff_delay(base_ms: u64, attempt: u32) -> Duration {
    Duration::from_millis(
        base_ms
            .saturating_mul(
                1u64.checked_shl(attempt.saturating_sub(1))
                    .unwrap_or(u64::MAX),
            )
            .min(300_000),
    )
}

pub fn target(config: &Config) -> Option<(String, String)> {
    let equip = config.equip_id().ok()?;
    let base = config.server.unc_base.trim();
    // Absolute local paths support the explicit CLI integration-test mode. The GUI
    // still rejects these through Config::validate's UNC-only rule.
    if base.is_empty() || (!Path::new(base).is_absolute() && !base.starts_with("\\\\")) {
        return None;
    }
    if base.starts_with("\\\\") && !crate::config::valid_unc(base) {
        return None;
    }
    Some((base.into(), equip))
}

pub fn test_unc(base: &str) -> Result<()> {
    anyhow::ensure!(
        crate::config::valid_unc(base)
            || (!base.starts_with("\\\\") && Path::new(base).is_absolute()),
        "connection test requires an absolute UNC or local test path"
    );
    let dir = Path::new(base).join("_status");
    fs::create_dir_all(&dir)
        .with_context(|| format!("연결 테스트 디렉터리 생성 실패: {}", dir.display()))?;
    let probe = dir.join(format!(
        ".probe-{}-{}.tmp",
        std::process::id(),
        Local::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let renamed = probe.with_extension("renamed.tmp");
    let result = (|| -> Result<()> {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&probe)?;
        f.write_all(b"csv-agent connection test\n")?;
        f.sync_all()?;
        drop(f);
        fs::rename(&probe, &renamed)
            .with_context(|| format!("연결 테스트 파일 이름 변경 실패: {}", probe.display()))?;
        fs::remove_file(&renamed)
            .with_context(|| format!("연결 테스트 파일 삭제 실패: {}", renamed.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&probe);
        let _ = fs::remove_file(&renamed);
    }
    result.with_context(|| format!("연결 테스트 실패: {}", dir.display()))
}

fn refresh_spool(spool: &Spool, shared: &SharedState) -> Result<()> {
    shared.refresh_spool(spool)?;
    Ok(())
}

pub async fn run(
    mut config: watch::Receiver<Arc<Config>>,
    spool: Arc<Spool>,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    nudge: Arc<Notify>,
    cancel: CancellationToken,
) {
    let mut failures = 0u32;
    let mut retry_at = None;
    let mut should_drain = true;
    loop {
        if cancel.is_cancelled() {
            return;
        }
        if target(&config.borrow()).is_none() {
            shared.update(|s| s.uploader = UploaderState::Gated);
            tokio::select! {_=cancel.cancelled()=>return,r=config.changed()=>{if r.is_err(){return;} should_drain=true; failures=0;}}
            continue;
        }
        if !should_drain {
            let delay = retry_at;
            tokio::select! {
                _=cancel.cancelled()=>return,
                r=config.changed()=>{if r.is_err(){return;} failures=0;},
                _=nudge.notified()=>{failures=0;},
                _=async {match delay {Some(at)=>tokio::time::sleep_until(at).await,None=>std::future::pending::<()>().await}}=>{},
            }
            retry_at = None;
        }
        should_drain = false;
        let Some((base, equip)) = target(&config.borrow()) else {
            continue;
        };
        shared.update(|s| s.uploader = UploaderState::Draining);
        let spool_worker = spool.clone();
        let db_worker = db.clone();
        let status = shared.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<bool> {
            let items = spool_worker.list_oldest_first()?;
            let Some(item) = items.into_iter().next() else {
                refresh_spool(&spool_worker, &status)?;
                return Ok(false);
            };
            let dest = Path::new(&base)
                .join("raw")
                .join(equip)
                .join(&item.date_dir)
                .join(&item.name);
            if let Some(bytes) = spool_worker
                .upload_item(&item, &dest)
                .with_context(|| format!("upload {}", dest.display()))?
            {
                let now = Local::now();
                let today = now.format("%Y%m%d").to_string();
                let mut date = status.counter_date.lock().unwrap();
                let prior = if *date == today {
                    status.snapshot().bytes_sent_today
                } else {
                    0
                };
                let total = prior.saturating_add(bytes);
                db_worker.set_counter("bytes_sent_date", &today)?;
                db_worker.set_counter("bytes_sent_today", &total.to_string())?;
                db_worker.set_counter("last_upload_at", &now.to_rfc3339())?;
                *date = today;
                status.update(|s| {
                    s.bytes_sent_today = total;
                    s.last_upload_at = Some(now);
                });
                tracing::info!(file=%item.name,bytes,"upload complete");
            }
            refresh_spool(&spool_worker, &status)?;
            Ok(true)
        })
        .await;
        match result.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok(more) => {
                failures = 0;
                should_drain = more;
                if !more {
                    shared.update(|s| s.uploader = UploaderState::Idle);
                }
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                shared.error("upload", crate::win::describe_error(&error));
                let cfg = config.borrow().clone();
                if failures > cfg.server.retry_max {
                    tracing::error!(failures, "upload suspended until a recovery signal");
                    shared.update(|s| s.uploader = UploaderState::Suspended);
                } else {
                    let until = tokio::time::Instant::now()
                        + backoff_delay(cfg.server.retry_base_ms, failures);
                    retry_at = Some(until);
                    shared.update(|s| {
                        s.uploader = UploaderState::Backoff {
                            until,
                            attempt: failures,
                        }
                    });
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn backoff_caps_without_overflow() {
        assert_eq!(backoff_delay(1000, 1), Duration::from_secs(1));
        assert_eq!(backoff_delay(1000, 3), Duration::from_secs(4));
        assert_eq!(backoff_delay(u64::MAX, 1000), Duration::from_secs(300));
    }
    #[test]
    fn connection_probe_leaves_no_files() {
        let dir = tempfile::tempdir().unwrap();
        test_unc(dir.path().to_str().unwrap()).unwrap();
        assert_eq!(fs::read_dir(dir.path().join("_status")).unwrap().count(), 0);
    }
}
