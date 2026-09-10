use crate::{
    bus::{FsEvent, WatchError},
    config::Config,
    paths::{normalize, CompiledPattern},
    shared::SharedState,
};
use anyhow::Result;
use notify::{
    event::{CreateKind, ModifyKind, RemoveKind, RenameMode},
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{mpsc, watch, Notify};
use tokio_util::sync::CancellationToken;

pub fn matches(config: &Config, path: &Path) -> bool {
    config
        .watch
        .iter()
        .filter(|p| p.enabled)
        .any(|p| CompiledPattern::new(p).is_ok_and(|p| p.matches(path)))
}

pub fn scan(pattern: &str) -> Result<Vec<PathBuf>> {
    let cfg = crate::config::WatchConfig {
        pattern: pattern.into(),
        ..Default::default()
    };
    let pattern = CompiledPattern::new(&cfg)?;
    let mut found = Vec::new();
    for entry in walkdir::WalkDir::new(&pattern.prefix).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() && pattern.matches(entry.path()) {
            found.push(entry.into_path());
        }
    }
    found.sort();
    found.dedup();
    Ok(found)
}

fn roots(patterns: &[CompiledPattern]) -> Vec<PathBuf> {
    let mut candidates: Vec<_> = patterns.iter().map(|p| p.prefix.clone()).collect();
    candidates.sort_by_key(|p| p.components().count());
    let mut roots: Vec<PathBuf> = vec![];
    for candidate in candidates {
        let normalized = normalize(&candidate).to_string();
        if !roots.iter().any(|root| {
            let root = normalize(root).to_string();
            normalized == root || normalized.starts_with(&(root + "/"))
        }) {
            roots.push(candidate);
        }
    }
    roots
}

fn build(
    config: &Config,
    events: mpsc::UnboundedSender<FsEvent>,
    rescan: Arc<Notify>,
    shared: Arc<SharedState>,
) -> (Option<RecommendedWatcher>, Vec<CompiledPattern>) {
    let mut patterns = Vec::new();
    let mut errors = Vec::new();
    for cfg in config.watch.iter().filter(|c| c.enabled) {
        match CompiledPattern::new(cfg) {
            Ok(p) => patterns.push(p),
            Err(e) => errors.push(WatchError {
                pattern: cfg.pattern.clone(),
                reason: e.to_string(),
            }),
        }
    }
    let callback_patterns = patterns.clone();
    let callback_shared = shared.clone();
    let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        let event = match event {
            Ok(e) => e,
            Err(e) => {
                callback_shared.error("watch", e.to_string());
                callback_shared.update(|s| {
                    s.watch_errors.push(WatchError {
                        pattern: "OS watcher".into(),
                        reason: e.to_string(),
                    })
                });
                rescan.notify_one();
                return;
            }
        };
        if event.need_rescan() {
            rescan.notify_one();
        }
        // Windows reports directory creation as Create(Any), and renames carry
        // no file type. Inspect only structural events so importing a populated
        // directory discovers its files even when no child events are emitted.
        let directory_change = match event.kind {
            EventKind::Create(CreateKind::Folder) | EventKind::Remove(RemoveKind::Folder) => true,
            EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Name(
                RenameMode::To | RenameMode::Both | RenameMode::Any,
            )) => event.paths.iter().any(|path| path.is_dir()),
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                let snapshot = callback_shared.snapshot();
                event.paths.iter().any(|path| {
                    let prefix = format!("{}/", normalize(path));
                    snapshot
                        .files
                        .iter()
                        .any(|file| normalize(&file.path).as_ref().starts_with(&prefix))
                })
            }
            _ => false,
        };
        if directory_change {
            rescan.notify_one();
        }
        let send = |path: PathBuf, removed: bool| {
            if callback_patterns.iter().any(|p| p.matches(&path)) {
                let _ = events.send(if removed {
                    FsEvent::Removed(path)
                } else {
                    FsEvent::Changed(path)
                });
            }
        };
        match event.kind {
            EventKind::Remove(_) | EventKind::Modify(ModifyKind::Name(RenameMode::From)) => {
                for path in event.paths {
                    send(path, true);
                }
            }
            EventKind::Modify(ModifyKind::Name(RenameMode::Both)) => {
                for (index, path) in event.paths.into_iter().enumerate() {
                    send(path, index == 0);
                }
            }
            EventKind::Create(_)
            | EventKind::Modify(ModifyKind::Data(_))
            | EventKind::Modify(ModifyKind::Any)
            | EventKind::Modify(ModifyKind::Name(_)) => {
                for path in event.paths {
                    send(path, false);
                }
            }
            _ => {}
        }
    });
    let watcher = match watcher {
        Ok(mut watcher) => {
            for root in roots(&patterns) {
                if let Err(error) = watcher.watch(&root, RecursiveMode::Recursive) {
                    for p in &patterns {
                        if p.prefix.starts_with(&root) {
                            errors.push(WatchError {
                                pattern: p.source.clone(),
                                reason: error.to_string(),
                            });
                        }
                    }
                }
            }
            Some(watcher)
        }
        Err(error) => {
            errors.push(WatchError {
                pattern: "OS watcher".into(),
                reason: error.to_string(),
            });
            None
        }
    };
    shared.update(|s| s.watch_errors = errors);
    (watcher, patterns)
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use crate::{config::WatchConfig, paths::AppPaths, runtime};
    use std::{fs, time::Duration};

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn moving_a_populated_directory_discovers_unchanged_csv_files() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("watched");
        let outside = temp.path().join("outside");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("ready.csv"), b"h\n0\n").unwrap();
        fs::write(outside.join("import.csv"), b"h\n1\n").unwrap();
        let paths = AppPaths::from_base(temp.path().join("agent"));
        let mut config = Config::default();
        config.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
        config.agent.autostart = false;
        config.agent.debounce_sec = 1;
        config.watch.push(WatchConfig {
            pattern: format!("{}/**/*.csv", root.display()),
            ..Default::default()
        });
        let handle = runtime::start(config, paths, Arc::new(|| {})).unwrap();
        let mut status = handle.status_rx.clone();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if status
                    .borrow_and_update()
                    .files
                    .iter()
                    .any(|f| f.offset == 4)
                {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        let imported = root.join("imported");
        fs::rename(outside, &imported).unwrap();
        // Do not touch the imported CSV: the directory event must initiate discovery.
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if status.borrow_and_update().files.iter().any(|f| {
                    normalize(&f.path) == normalize(&imported.join("import.csv"))
                        && f.offset == 4
                        && f.present
                }) {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        fs::rename(&imported, temp.path().join("removed-again")).unwrap();
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                if status.borrow_and_update().files.iter().any(|f| {
                    normalize(&f.path) == normalize(&imported.join("import.csv")) && !f.present
                }) {
                    break;
                }
                status.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        handle.shutdown().unwrap();
    }
}

async fn initial_scan(
    patterns: Vec<CompiledPattern>,
    events: mpsc::UnboundedSender<FsEvent>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
) {
    let error_shared = shared.clone();
    let result = tokio::task::spawn_blocking(move || {
        let mut found = HashSet::new();
        for root in roots(&patterns) {
            for entry in walkdir::WalkDir::new(root).follow_links(false) {
                if cancel.is_cancelled() {
                    return;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error=%e,"scan entry inaccessible");
                        continue;
                    }
                };
                if entry.file_type().is_file() && patterns.iter().any(|p| p.matches(entry.path())) {
                    found.insert(normalize(entry.path()).to_string());
                    let _ = events.send(FsEvent::Changed(entry.into_path()));
                }
            }
        }
        shared.update(|s| {
            for file in &mut s.files {
                file.present = found.contains(&normalize(&file.path).to_string());
            }
        });
    })
    .await;
    if let Err(e) = result {
        error_shared.error("watch", e.to_string());
    }
}

pub async fn run(
    mut config: watch::Receiver<Arc<Config>>,
    events: mpsc::UnboundedSender<FsEvent>,
    nudge: Arc<Notify>,
    shared: Arc<SharedState>,
    cancel: CancellationToken,
) {
    let rescan = Arc::new(Notify::new());
    let mut active = config.borrow().clone();
    let (mut watcher, mut patterns) =
        build(&active, events.clone(), rescan.clone(), shared.clone());
    initial_scan(
        patterns.clone(),
        events.clone(),
        shared.clone(),
        cancel.clone(),
    )
    .await;
    loop {
        tokio::select! {
            _=cancel.cancelled()=>break,
            changed=config.changed()=>{
                if changed.is_err(){break;}
                let new=config.borrow().clone();
                if new.watch!=active.watch {
                    drop(watcher.take());
                    (watcher,patterns)=build(&new,events.clone(),rescan.clone(),shared.clone());
                    initial_scan(patterns.clone(),events.clone(),shared.clone(),cancel.clone()).await;
                }
                active=new;
            }
            _=rescan.notified()=>initial_scan(patterns.clone(),events.clone(),shared.clone(),cancel.clone()).await,
            _=nudge.notified()=>{
                if !shared.snapshot().watch_errors.is_empty() {
                    drop(watcher.take());
                    (watcher,patterns)=build(&active,events.clone(),rescan.clone(),shared.clone());
                    initial_scan(patterns.clone(),events.clone(),shared.clone(),cancel.clone()).await;
                }
            }
        }
    }
}
