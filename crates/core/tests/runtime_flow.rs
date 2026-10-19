use data_relay_core::{
    bus::{Command, StatusSnapshot, UploaderState},
    config::{Config, WatchConfig},
    paths::AppPaths,
    runtime::{self, AgentHandle},
};
use flate2::read::GzDecoder;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

fn fixture(root: &Path) -> (Config, AppPaths, PathBuf) {
    let paths = AppPaths::from_base(root.join("agent"));
    let input = root.join("input");
    fs::create_dir_all(&input).unwrap();
    let mut cfg = Config::default();
    cfg.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
    cfg.log.dir = paths.logs.to_string_lossy().into_owned();
    cfg.agent.autostart = false;
    cfg.agent.debounce_sec = 1;
    cfg.agent.max_wait_sec = 2;
    cfg.server.unc_base = root.join("server").to_string_lossy().into_owned();
    cfg.equip.num = "TEST-1".into();
    cfg.equip.name = "fixture".into();
    cfg.watch.push(WatchConfig {
        pattern: format!("{}/*.csv", input.display()),
        ..Default::default()
    });
    (cfg, paths, input.join("data.csv"))
}

async fn until(handle: &AgentHandle, predicate: impl Fn(&StatusSnapshot) -> bool) {
    let mut status = handle.status_rx.clone();
    let result = tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            if predicate(&status.borrow_and_update()) {
                break;
            }
            status
                .changed()
                .await
                .expect("runtime status channel closed");
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "timed out; latest state: {:?}",
        handle.snapshot()
    );
}

fn chunks(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut result = Vec::new();
    if !root.exists() {
        return result;
    }
    for entry in walkdir::WalkDir::new(root) {
        let entry = entry.unwrap();
        if !entry.file_type().is_file() || !entry.file_name().to_string_lossy().ends_with(".csv.gz")
        {
            continue;
        }
        let mut bytes = Vec::new();
        GzDecoder::new(fs::File::open(entry.path()).unwrap())
            .read_to_end(&mut bytes)
            .unwrap();
        result.push((entry.file_name().to_string_lossy().into_owned(), bytes));
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_scan_partial_append_offline_restart_and_upload() {
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, paths, source) = fixture(temp.path());
    cfg.equip.name.clear(); // Collection continues while identity gates upload.
    fs::write(&source, b"t,v\n1,2\npar").unwrap();
    let handle = runtime::start(cfg.clone(), paths.clone(), Arc::new(|| {})).unwrap();
    until(&handle, |s| {
        s.spool_pending == 1 && s.files.iter().any(|f| f.offset == 8)
    })
    .await;
    assert_eq!(chunks(&paths.spool)[0].1, b"t,v\n1,2\n");
    let mut file = fs::OpenOptions::new().append(true).open(&source).unwrap();
    file.write_all(b"tial,3\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    until(&handle, |s| {
        s.spool_pending == 2 && s.files.iter().any(|f| f.offset == 18)
    })
    .await;
    let spooled = chunks(&paths.spool);
    assert!(spooled
        .iter()
        .any(|(_, bytes)| bytes == b"t,v\npartial,3\n"));
    handle.shutdown().unwrap();

    cfg.equip.name = "fixture".into();
    let handle = runtime::start(cfg.clone(), paths.clone(), Arc::new(|| {})).unwrap();
    until(&handle, |s| {
        s.spool_pending == 0 && s.bytes_sent_today > 0 && s.uploader == UploaderState::Idle
    })
    .await;
    let first_total = handle.snapshot().bytes_sent_today;
    assert_eq!(
        chunks(Path::new(&cfg.server.unc_base).join("raw").as_path()),
        spooled
    );
    let key = handle.snapshot().files[0].file_key.clone();
    handle.send(Command::ResetOffset(key)).unwrap();
    until(&handle, |s| {
        s.bytes_sent_today > first_total
            && s.spool_pending == 0
            && s.uploader == UploaderState::Idle
    })
    .await;
    // Explicit replay may use a larger complete range, but must preserve source
    // bytes and retain already delivered incremental chunks.
    let replayed = chunks(Path::new(&cfg.server.unc_base));
    assert!(replayed
        .iter()
        .any(|(_, data)| data == b"t,v\n1,2\npartial,3\n"));
    let bytes_sent = handle.snapshot().bytes_sent_today;
    handle.shutdown().unwrap();

    // A further restart reloads the committed offset and persisted counters.
    let handle = runtime::start(cfg, paths, Arc::new(|| {})).unwrap();
    until(&handle, |s| s.files.iter().any(|f| f.offset == 18)).await;
    assert_eq!(handle.snapshot().bytes_sent_today, bytes_sent);
    handle.shutdown().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn upload_suspends_then_manual_signal_recovers_without_loss() {
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, paths, source) = fixture(temp.path());
    cfg.server.retry_max = 0;
    fs::write(
        &cfg.server.unc_base,
        b"destination is temporarily unavailable",
    )
    .unwrap();
    fs::write(&source, b"a,b\n10,20\n").unwrap();
    let handle = runtime::start(cfg.clone(), paths, Arc::new(|| {})).unwrap();
    until(&handle, |s| {
        s.spool_pending == 1 && s.uploader == UploaderState::Suspended
    })
    .await;
    fs::remove_file(&cfg.server.unc_base).unwrap();
    fs::create_dir_all(&cfg.server.unc_base).unwrap();
    handle.send(Command::FlushNow).unwrap();
    until(&handle, |s| {
        s.spool_pending == 0 && s.bytes_sent_today > 0 && s.uploader == UploaderState::Idle
    })
    .await;
    assert_eq!(
        chunks(Path::new(&cfg.server.unc_base))[0].1,
        b"a,b\n10,20\n"
    );
    handle.shutdown().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn successful_active_connection_test_resumes_suspended_uploads() {
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, paths, source) = fixture(temp.path());
    cfg.server.retry_max = 0;
    fs::write(&cfg.server.unc_base, b"unavailable").unwrap();
    fs::write(&source, b"a,b\n10,20\n").unwrap();
    let handle = runtime::start(cfg.clone(), paths, Arc::new(|| {})).unwrap();
    until(&handle, |s| {
        s.spool_pending == 1 && s.uploader == UploaderState::Suspended
    })
    .await;
    fs::remove_file(&cfg.server.unc_base).unwrap();
    fs::create_dir_all(&cfg.server.unc_base).unwrap();
    let (reply, result) = tokio::sync::oneshot::channel();
    handle
        .send(Command::TestUnc {
            base: cfg.server.unc_base.clone(),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    until(&handle, |s| {
        s.spool_pending == 0 && s.bytes_sent_today > 0 && s.uploader == UploaderState::Idle
    })
    .await;
    assert_eq!(
        chunks(Path::new(&cfg.server.unc_base))[0].1,
        b"a,b\n10,20\n"
    );
    handle.shutdown().unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn testing_an_unsaved_destination_does_not_apply_or_clear_saved_settings() {
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, paths, _) = fixture(temp.path());
    cfg.equip.num.clear();
    cfg.equip.name.clear();
    cfg.server.unc_base.clear();
    let handle = runtime::start(cfg.clone(), paths, Arc::new(|| {})).unwrap();
    until(&handle, |s| s.uploader == UploaderState::Gated).await;
    let errors = handle.snapshot().config_errors.clone();
    let (reply, result) = tokio::sync::oneshot::channel();
    handle
        .send(Command::TestUnc {
            base: temp
                .path()
                .join("new-destination")
                .to_string_lossy()
                .into_owned(),
            reply,
        })
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), result)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(&**handle.config_rx.borrow(), &cfg);
    assert_eq!(handle.snapshot().config_errors, errors);
    assert_eq!(handle.snapshot().uploader, UploaderState::Gated);
    handle.shutdown().unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_state_directory_cannot_be_started_twice() {
    let temp = tempfile::tempdir().unwrap();
    let (cfg, paths, _) = fixture(temp.path());
    let first = runtime::start(cfg.clone(), paths.clone(), Arc::new(|| {})).unwrap();
    let second = runtime::start(cfg, paths, Arc::new(|| {}));
    assert!(
        second.is_err(),
        "a second owner would corrupt offsets/spool"
    );
    first.shutdown().unwrap();
}

#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_pattern_reload_and_manual_task_capture() {
    use data_relay_core::config::TaskConfig;
    let temp = tempfile::tempdir().unwrap();
    let (mut cfg, paths, source) = fixture(temp.path());
    cfg.watch.clear();
    cfg.task.push(TaskConfig {
        name: "echo_test".into(),
        program: "cmd.exe".into(),
        args: vec!["/D".into(), "/C".into(), "echo runtime-output".into()],
        ..Default::default()
    });
    fs::write(&source, b"a,b\n3,4\n").unwrap();
    let handle = runtime::start(cfg.clone(), paths.clone(), Arc::new(|| {})).unwrap();
    cfg.watch.push(WatchConfig {
        pattern: source.to_string_lossy().into_owned(),
        ..Default::default()
    });
    handle.send(Command::ReloadConfig(cfg)).unwrap();
    handle
        .send(Command::RunTaskNow("echo_test".into()))
        .unwrap();
    until(&handle, |s| {
        s.files.iter().any(|f| f.offset == 8)
            && s.tasks
                .iter()
                .any(|t| t.last.as_ref().is_some_and(|r| r.exit_code == Some(0)))
    })
    .await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    handle
        .send(Command::TaskHistory {
            name: "echo_test".into(),
            reply: tx,
        })
        .unwrap();
    let history = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(history[0]
        .stdout_tail
        .as_deref()
        .unwrap()
        .contains("runtime-output"));
    handle.shutdown().unwrap();
}
