use crate::{
    bus::{Command, FileStatus, ProcessRequest, Reason, StatusSnapshot, TaskStatus, UploaderState},
    config::Config,
    paths::{normalize, AppPaths},
    shared::SharedState,
    spool::Spool,
    state::StateDb,
};
use anyhow::Result;
use chrono::{DateTime, Local};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, watch, Notify},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub struct AgentHandle {
    pub cmd_tx: mpsc::UnboundedSender<Command>,
    pub status_rx: watch::Receiver<Arc<StatusSnapshot>>,
    pub config_rx: watch::Receiver<Arc<Config>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl AgentHandle {
    pub fn send(&self, command: Command) -> std::result::Result<(), String> {
        self.cmd_tx
            .send(command)
            .map_err(|_| "agent runtime has stopped".into())
    }
    pub fn snapshot(&self) -> Arc<StatusSnapshot> {
        self.status_rx.borrow().clone()
    }
    pub fn shutdown(mut self) -> std::result::Result<(), String> {
        let _ = self.cmd_tx.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| "agent runtime panicked".to_string())?;
        }
        Ok(())
    }
}
impl Drop for AgentHandle {
    fn drop(&mut self) {
        let _ = self.cmd_tx.send(Command::Shutdown);
    }
}

pub fn start(
    config: Config,
    paths: AppPaths,
    on_change: Arc<dyn Fn() + Send + Sync>,
) -> Result<AgentHandle> {
    start_impl(config, paths, on_change, false)
}
pub fn start_dry_run(
    config: Config,
    paths: AppPaths,
    on_change: Arc<dyn Fn() + Send + Sync>,
) -> Result<AgentHandle> {
    start_impl(config, paths, on_change, true)
}

fn start_impl(
    config: Config,
    paths: AppPaths,
    on_change: Arc<dyn Fn() + Send + Sync>,
    dry_run: bool,
) -> Result<AgentHandle> {
    std::fs::create_dir_all(&paths.base)?;
    let guard = crate::win::InstanceLock::acquire(&paths.state)?;
    let spool_path = if config.agent.spool_dir.trim().is_empty() {
        paths.spool.clone()
    } else {
        PathBuf::from(&config.agent.spool_dir)
    };
    let spool_guard = crate::win::InstanceLock::acquire(&spool_path.join(".csv-agent-spool-lock"))?;
    let db = Arc::new(StateDb::open(&paths.state)?);
    let spool = Arc::new(Spool::new(spool_path)?);
    let (shared, status_rx) = SharedState::new(&config, on_change);
    *shared.lifetime_guards.lock().unwrap() = vec![guard, spool_guard];
    shared.refresh_progress(db.load_all_progress()?);
    let tasks = config
        .task
        .iter()
        .map(|task| {
            Ok(TaskStatus {
                name: task.name.clone(),
                enabled: task.enabled,
                next_run: None,
                running: false,
                last: db.recent_task_runs(&task.name, 1)?.into_iter().next(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    shared.update(|s| s.tasks = tasks);
    if let Some(error) = &db.recovery_warning {
        shared.error("state", error.clone());
    }
    let stats = spool.stats()?;
    let date = db.get_counter("bytes_sent_date")?.unwrap_or_default();
    let today = Local::now().format("%Y%m%d").to_string();
    let sent = if date == today {
        db.get_counter("bytes_sent_today")?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
    } else {
        0
    };
    let last = db
        .get_counter("last_upload_at")?
        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
        .map(|t| t.with_timezone(&Local));
    shared.update(|s| {
        s.spool_pending = stats.pending;
        s.spool_bytes = stats.bytes;
        s.bytes_sent_today = sent;
        s.last_upload_at = last;
    });
    let previous = db.get_meta("last_equip_id")?;
    if let Ok(id) = config.equip_id() {
        if previous.as_ref().is_some_and(|old| old != &id) {
            *shared.equip_id_changed_from.lock().unwrap() = previous;
        }
        db.set_meta("last_equip_id", &id)?;
    }
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let (config_tx, config_rx) = watch::channel(Arc::new(config));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("csv-agent-worker")
        .build()?;
    let config_worker = config_rx.clone();
    let thread = std::thread::Builder::new()
        .name("agent-runtime".into())
        .spawn(move || {
            runtime.block_on(run(
                cmd_rx,
                config_tx,
                config_worker,
                db,
                spool,
                shared,
                dry_run,
            ));
            // SMB calls cannot be forcibly cancelled. Any detached blocking call owns
            // SharedState, which retains the instance lock until its IO has finished.
            runtime.shutdown_timeout(Duration::from_secs(1));
        })?;
    Ok(AgentHandle {
        cmd_tx,
        status_rx,
        config_rx,
        thread: Some(thread),
    })
}

#[allow(clippy::too_many_arguments)] // Internal worker wiring mirrors the documented channel boundary.
async fn chunk_worker(
    mut requests: mpsc::Receiver<ProcessRequest>,
    config: watch::Receiver<Arc<Config>>,
    db: Arc<StateDb>,
    spool: Arc<Spool>,
    shared: Arc<SharedState>,
    upload: Arc<Notify>,
    cancel: CancellationToken,
    dry_run: bool,
) {
    loop {
        let request = tokio::select! {biased;_=cancel.cancelled()=>break,request=requests.recv()=>match request{Some(r)=>r,None=>break}};
        if dry_run {
            tracing::info!(?request, "dry run: observed file event");
            continue;
        }
        let cfg = config.borrow().clone();
        let database = db.clone();
        let queue = spool.clone();
        let status = shared.clone();
        let nudge = upload.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            let path: Option<PathBuf> = match request {
                ProcessRequest::Removed(path) => {
                    database.delete_progress_by_path(normalize(&path).as_ref())?;
                    None
                }
                ProcessRequest::Reset(key) => database
                    .reset_offset(&key)?
                    .map(|r| PathBuf::from(r.display_path)),
                ProcessRequest::Process { path, reason } => {
                    tracing::debug!(?path, ?reason, "processing file");
                    Some(path)
                }
            };
            if let Some(path) = path {
                let result = crate::chunk::process_file(&database, &queue, &cfg, &path);
                status.refresh_progress(database.load_all_progress()?);
                match result {
                    Ok(outcome) => {
                        tracing::debug!(?path, ?outcome, "file processed");
                        status.update(|s| {
                            for f in &mut s.files {
                                if normalize(&f.path) == normalize(&path) {
                                    f.present = true;
                                    f.last_error = None;
                                }
                            }
                        });
                        if matches!(outcome, crate::chunk::Outcome::Chunked { .. }) {
                            nudge.notify_one();
                        }
                    }
                    Err(error) => {
                        let message = crate::win::describe_error(&error);
                        status.error("chunk", format!("{}: {message}", path.display()));
                        status.update(|s| {
                            if let Some(file) = s
                                .files
                                .iter_mut()
                                .find(|f| normalize(&f.path) == normalize(&path))
                            {
                                file.last_error = Some(message.clone());
                            } else {
                                s.files.push(FileStatus {
                                    file_key: format!("pending:{}", normalize(&path)),
                                    path: path.clone(),
                                    size: 0,
                                    offset: 0,
                                    header_kind: None,
                                    header_mode: cfg
                                        .watch
                                        .iter()
                                        .find(|w| {
                                            crate::paths::CompiledPattern::new(w)
                                                .is_ok_and(|p| p.matches(&path))
                                        })
                                        .map(|w| w.header)
                                        .unwrap_or_default(),
                                    last_processed: None,
                                    present: true,
                                    last_error: Some(message),
                                });
                            }
                        });
                    }
                }
            } else {
                status.refresh_progress(database.load_all_progress()?);
            }
            if status.refresh_spool(&queue)? > 0 {
                nudge.notify_one();
            }
            Ok(())
        })
        .await;
        if let Err(e) = result.map_err(anyhow::Error::from).and_then(|r| r) {
            shared.error("chunk", crate::win::describe_error(&e));
        }
    }
}

async fn reload_effects(
    mut requests: mpsc::UnboundedReceiver<(Arc<Config>, Arc<Config>)>,
    db: Arc<StateDb>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
) {
    loop {
        let (old, config) = tokio::select! {_=cancel.cancelled()=>break,r=requests.recv()=>match r{Some(r)=>r,None=>break}};
        let database = db.clone();
        let status = shared.clone();
        let result = tokio::task::spawn_blocking(move || -> Result<()> {
            // Keep the data locks alive even if a blocking OS operation outlives shutdown.
            let _lifetime = status;
            if old.agent.autostart != config.agent.autostart {
                crate::win::set_autostart(config.agent.autostart)?;
            }
            if old.log.level != config.log.level {
                crate::logging::reload_level(&config.log.level)?;
            }
            if let Ok(id) = config.equip_id() {
                database.set_meta("last_equip_id", &id)?;
            }
            Ok(())
        })
        .await;
        if let Err(e) = result.map_err(anyhow::Error::from).and_then(|r| r) {
            shared.error("config", crate::win::describe_error(&e));
        }
    }
}

async fn run(
    mut commands: mpsc::UnboundedReceiver<Command>,
    config_tx: watch::Sender<Arc<Config>>,
    config_rx: watch::Receiver<Arc<Config>>,
    db: Arc<StateDb>,
    spool: Arc<Spool>,
    shared: Arc<SharedState>,
    dry_run: bool,
) {
    let cancel = CancellationToken::new();
    let (fs_tx, fs_rx) = mpsc::unbounded_channel();
    let (process_tx, process_rx) = mpsc::channel(1024);
    let (task_tx, task_rx) = mpsc::unbounded_channel();
    let (reload_tx, reload_rx) = mpsc::unbounded_channel();
    let uploader = Arc::new(Notify::new());
    let watcher = Arc::new(Notify::new());
    let task_resume = Arc::new(Notify::new());
    let heartbeat_resume = Arc::new(Notify::new());
    let mut workers = JoinSet::new();
    workers.spawn(reload_effects(
        reload_rx,
        db.clone(),
        shared.clone(),
        cancel.clone(),
    ));
    workers.spawn(crate::watch::run(
        config_rx.clone(),
        fs_tx,
        watcher.clone(),
        shared.clone(),
        cancel.clone(),
    ));
    workers.spawn(crate::debounce::run(
        fs_rx,
        process_tx.clone(),
        config_rx.clone(),
        cancel.clone(),
    ));
    workers.spawn(chunk_worker(
        process_rx,
        config_rx.clone(),
        db.clone(),
        spool.clone(),
        shared.clone(),
        uploader.clone(),
        cancel.clone(),
        dry_run,
    ));
    if !dry_run {
        workers.spawn(crate::upload::run(
            config_rx.clone(),
            spool,
            db.clone(),
            shared.clone(),
            uploader.clone(),
            cancel.clone(),
        ));
        workers.spawn(crate::heartbeat::run(
            config_rx.clone(),
            shared.clone(),
            uploader.clone(),
            watcher.clone(),
            heartbeat_resume.clone(),
            cancel.clone(),
        ));
        workers.spawn(crate::task::run(
            config_rx.clone(),
            task_rx,
            db.clone(),
            shared.clone(),
            task_resume.clone(),
            cancel.clone(),
        ));
    } else {
        shared.update(|s| s.uploader = UploaderState::Gated);
    }
    let mut replies = JoinSet::new();
    let mut manual_pending = std::collections::VecDeque::new();
    tracing::info!(dry_run, "agent runtime started");
    loop {
        tokio::select! {
            result=workers.join_next(),if !workers.is_empty()=>{
                shared.error("runtime",format!("worker exited unexpectedly: {result:?}"));break;
            }
            _=replies.join_next(),if !replies.is_empty()=>{},
            permit=process_tx.reserve(),if !manual_pending.is_empty()=>{
                if let Ok(permit)=permit {permit.send(manual_pending.pop_front().unwrap());}
            }
            command=commands.recv()=>match command {
                None|Some(Command::Shutdown)=>break,
                Some(Command::ReloadConfig(config))=>{
                    let old=config_rx.borrow().clone();let config=Arc::new(config);
                    shared.apply_config(&config);config_tx.send_replace(config.clone());
                    let _=reload_tx.send((old,config));
                }
                Some(Command::ConfigError(message))=>shared.update(|s|s.config_errors.push(message)),
                Some(Command::FlushNow)=>uploader.notify_one(),
                Some(Command::ProcessFileNow(path))=>{
                    manual_pending.push_back(ProcessRequest::Process{path,reason:Reason::Manual});
                }
                Some(Command::ResetOffset(key))=>{
                    manual_pending.push_back(ProcessRequest::Reset(key));
                }
                Some(Command::RunTaskNow(name))=>{let _=task_tx.send(crate::task::TaskRequest::RunSaved(name));}
                Some(Command::TestTask{config,reply})=>{
                    if dry_run {let _=reply.send(Err("파일 감시 dry-run 모드에서는 작업을 실행하지 않습니다.".into()));shared.repaint();}
                    else if let Err(error)=task_tx.send(crate::task::TaskRequest::Test{config,reply}) {
                        if let crate::task::TaskRequest::Test{reply,..}=error.0 {let _=reply.send(Err("작업 실행기가 종료되었습니다.".into()));shared.repaint();}
                    }
                }
                Some(Command::SystemResumed)=>{uploader.notify_one();task_resume.notify_one();heartbeat_resume.notify_one();watcher.notify_one();}
                Some(Command::TestUnc{base,reply})=>{
                    let status=shared.clone();
                    let current_config=config_rx.clone();
                    let upload_nudge=uploader.clone();
                    let heartbeat_nudge=heartbeat_resume.clone();
                    let watch_nudge=watcher.clone();
                    replies.spawn(async move {
                        let tested_base=base.clone();
                        let result=tokio::task::spawn_blocking(move||crate::upload::test_unc(&base)).await.map_err(anyhow::Error::from).and_then(|r|r).map_err(|e|crate::win::describe_error(&e));
                        // A successful probe of the active target is an explicit
                        // recovery signal. Testing an unsaved draft must not apply
                        // that configuration or clear unrelated runtime errors.
                        if result.is_ok() && normalize(std::path::Path::new(&tested_base)) == normalize(std::path::Path::new(&current_config.borrow().server.unc_base)) {
                            upload_nudge.notify_one();
                            heartbeat_nudge.notify_one();
                            watch_nudge.notify_one();
                        }
                        let _=reply.send(result);status.repaint();
                    });
                }
                Some(Command::PreviewPattern{pattern,reply})=>{
                    let status=shared.clone();replies.spawn(async move {
                        let result=tokio::task::spawn_blocking(move||crate::watch::scan(&pattern)).await.map_err(anyhow::Error::from).and_then(|r|r).map_err(|e|crate::win::describe_error(&e));
                        let _=reply.send(result);status.repaint();
                    });
                }
                Some(Command::TaskHistory{name,reply})=>{
                    let status=shared.clone();let database=db.clone();replies.spawn(async move {
                        let result=tokio::task::spawn_blocking(move||database.recent_task_runs(&name,20)).await;
                        match result.map_err(anyhow::Error::from).and_then(|r|r) {Ok(rows)=>{let _=reply.send(rows);},Err(e)=>{status.error("state",e.to_string());let _=reply.send(vec![]);}}
                        status.repaint();
                    });
                }
            }
        }
    }
    cancel.cancel();
    drop(process_tx);
    drop(task_tx);
    let finish = async {
        while workers.join_next().await.is_some() {}
        while replies.join_next().await.is_some() {}
    };
    if tokio::time::timeout(Duration::from_secs(10), finish)
        .await
        .is_err()
    {
        tracing::warn!(
            "shutdown grace period elapsed; unfinished blocking IO retains instance lock"
        );
        workers.abort_all();
        replies.abort_all();
    }
    tracing::info!("agent runtime stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn gated_identity_still_recovers_missing_watch_and_locks_custom_spool() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_base(dir.path().join("agent"));
        let source = dir.path().join("not-yet-present");
        let custom = dir.path().join("custom-spool");
        let mut cfg = Config::default();
        cfg.agent.spool_dir = custom.to_string_lossy().into_owned();
        cfg.agent.heartbeat_interval_sec = 1;
        cfg.agent.debounce_sec = 1;
        cfg.agent.max_wait_sec = 2;
        cfg.agent.autostart = false;
        cfg.watch.push(crate::config::WatchConfig {
            pattern: format!("{}/*.csv", source.display()),
            ..Default::default()
        });
        let agent = start(cfg.clone(), paths, Arc::new(|| {})).unwrap();
        let mut status = agent.status_rx.clone();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if !status.borrow_and_update().watch_errors.is_empty() {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(start(
            cfg,
            AppPaths::from_base(dir.path().join("other-agent")),
            Arc::new(|| {})
        )
        .is_err());
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("data.csv"), b"a,b\n1,2\n").unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if status.borrow_and_update().spool_pending == 1 {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(agent.snapshot().watch_errors.is_empty());
        assert_eq!(
            Spool::new(custom)
                .unwrap()
                .list_oldest_first()
                .unwrap()
                .len(),
            1
        );
        agent.shutdown().unwrap();
    }
}
