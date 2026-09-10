use crate::{config::Config, shared::SharedState, upload};
use anyhow::{Context, Result};
use chrono::Local;
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{watch, Notify};
use tokio_util::sync::CancellationToken;

pub fn write_status(config: &Config, shared: &SharedState) -> Result<()> {
    let (base, equip) = upload::target(config).ok_or_else(|| anyhow::anyhow!("heartbeat gated"))?;
    let today = Local::now().format("%Y%m%d").to_string();
    {
        let mut date = shared.counter_date.lock().unwrap();
        if *date != today {
            *date = today;
            shared.update(|s| s.bytes_sent_today = 0);
        }
    }
    let snapshot = shared.snapshot();
    let changed_from = shared.equip_id_changed_from.lock().unwrap().clone();
    let data = serde_json::json!({
        "equip_num":config.equip.num, "equip_name":config.equip.name,"equip_id":equip,
        "hostname":hostname::get().unwrap_or_default().to_string_lossy(),
        "agent_version":env!("CARGO_PKG_VERSION"),"timestamp":Local::now().to_rfc3339(),
        "agent_started_at":shared.started_at.to_rfc3339(),"equip_id_changed_from":changed_from,
        "watched_files":snapshot.files.iter().filter(|f|f.present).count(),
        "spool_pending":snapshot.spool_pending,"spool_bytes":snapshot.spool_bytes,
        "bytes_sent_today":snapshot.bytes_sent_today,
        "last_error":snapshot.last_error.as_ref().map(|e|serde_json::json!({"at":e.at.to_rfc3339(),"component":e.component,"message":e.message})),
        "config_errors":snapshot.config_errors,"watch_errors":snapshot.watch_errors,
        "uploader_state":snapshot.uploader.as_str(),
        "tasks":snapshot.tasks.iter().map(|t|serde_json::json!({
            "name":t.name,"last_run":t.last.as_ref().map(|r|&r.started_at),
            "exit_code":t.last.as_ref().and_then(|r|r.exit_code),
            "duration_sec":t.last.as_ref().and_then(|r|r.duration_ms).map(|ms|ms as f64 /1000.0)
        })).collect::<Vec<_>>()
    });
    let dir = Path::new(&base).join("_status");
    fs::create_dir_all(&dir)
        .with_context(|| format!("하트비트 디렉터리 생성 실패: {}", dir.display()))?;
    let dest = dir.join(format!("{equip}.json"));
    let tmp = dir.join(format!("{equip}.json.tmp"));
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(&serde_json::to_vec_pretty(&data)?)?;
        file.sync_all()?;
        drop(file);
        crate::win::replace_file(&tmp, &dest)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result.with_context(|| format!("하트비트 파일 쓰기 실패: {}", dest.display()))?;
    let mut previous = shared.equip_id_changed_from.lock().unwrap();
    if *previous == changed_from {
        *previous = None;
    }
    Ok(())
}

pub async fn run(
    mut config: watch::Receiver<Arc<Config>>,
    shared: Arc<SharedState>,
    uploader: Arc<Notify>,
    watcher: Arc<Notify>,
    resume: Arc<Notify>,
    cancel: CancellationToken,
) {
    let mut next = tokio::time::Instant::now();
    let mut last_warning = None;
    let mut active = config.borrow().clone();
    loop {
        tokio::select! {
            _=cancel.cancelled()=>return,
            r=config.changed()=>{
                if r.is_err(){return;}
                let new=config.borrow().clone();
                if new.equip!=active.equip || new.server!=active.server {next=tokio::time::Instant::now();}
                else if new.agent.heartbeat_interval_sec!=active.agent.heartbeat_interval_sec {
                    next=tokio::time::Instant::now()+Duration::from_secs(new.agent.heartbeat_interval_sec.max(1));
                }
                active=new;continue;
            },
            _=resume.notified()=>{},
            _=tokio::time::sleep_until(next)=>{},
        }
        let cfg = config.borrow().clone();
        let status = shared.clone();
        if upload::target(&cfg).is_none() {
            if !shared.snapshot().watch_errors.is_empty() {
                watcher.notify_one();
            }
            next = tokio::time::Instant::now()
                + Duration::from_secs(cfg.agent.heartbeat_interval_sec.max(1));
            continue;
        }
        let result = tokio::task::spawn_blocking(move || write_status(&cfg, &status)).await;
        match result.map_err(anyhow::Error::from).and_then(|r| r) {
            Ok(()) => {
                uploader.notify_one();
                last_warning = None;
            }
            Err(e) => {
                let now = tokio::time::Instant::now();
                if last_warning
                    .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(600))
                {
                    shared.error("heartbeat", crate::win::describe_error(&e));
                    last_warning = Some(now);
                }
            }
        }
        // Local directory availability is independent of network availability.
        if !shared.snapshot().watch_errors.is_empty() {
            watcher.notify_one();
        }
        next = tokio::time::Instant::now()
            + Duration::from_secs(config.borrow().agent.heartbeat_interval_sec.max(1));
    }
}
