use csv_agent_core::{
    config::{self, Config, TaskConfig, WatchConfig},
    state::StateDb,
};
use std::{
    fs,
    process::{Command, Output},
};

fn cli(args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_csv-agent-cli"));
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.output().unwrap()
}

#[test]
fn example_config_and_chunk_name_diagnostics() {
    let example: Config = toml::from_str(include_str!("../../../config.example.toml")).unwrap();
    assert!(example.validate().is_empty(), "{:?}", example.validate());
    let output = cli(&[
        "chunk-name",
        "--parse",
        "nested~a__b.csv__12-45__syn.csv.gz",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["start"], 12);
    assert_eq!(result["name_seg"], "nested~a__b.csv");
    assert!(!cli(&["chunk-name", "--parse", "invalid.csv.gz"])
        .status
        .success());
}

#[test]
fn malformed_config_is_rejected_without_overwriting_it() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.toml");
    fs::write(&path, "[broken").unwrap();
    let output = cli(&["--data-dir", temp.path().to_str().unwrap(), "check-config"]);
    assert!(!output.status.success());
    assert_eq!(fs::read_to_string(path).unwrap(), "[broken");
}

#[test]
fn dry_run_does_not_collect_upload_or_execute_commands() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("input.csv");
    fs::write(&source, b"t,v\n1,2\n").unwrap();
    let mut cfg = Config::default();
    cfg.equip.num = "TEST".into();
    cfg.equip.name = "dry-run".into();
    cfg.agent.autostart = false;
    cfg.agent.debounce_sec = 1;
    cfg.agent.spool_dir = temp.path().join("spool").to_string_lossy().into_owned();
    cfg.log.dir = temp.path().join("logs").to_string_lossy().into_owned();
    cfg.server.unc_base = temp
        .path()
        .join("destination")
        .to_string_lossy()
        .into_owned();
    cfg.watch.push(WatchConfig {
        pattern: source.to_string_lossy().into_owned(),
        ..Default::default()
    });
    cfg.task.push(TaskConfig {
        name: "must_not_run".into(),
        program: "missing-program.exe".into(),
        run_on_start: true,
        ..Default::default()
    });
    config::save(&temp.path().join("config.toml"), &cfg).unwrap();
    let output = cli(&[
        "--data-dir",
        temp.path().to_str().unwrap(),
        "--allow-local-target",
        "run",
        "--dry-run",
        "--duration-sec",
        "2",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = StateDb::open(&temp.path().join("state.db")).unwrap();
    assert!(db.load_all_progress().unwrap().is_empty());
    assert!(db.recent_task_runs("must_not_run", 20).unwrap().is_empty());
    assert!(!temp.path().join("destination").exists());
    assert_eq!(fs::read_dir(&cfg.agent.spool_dir).unwrap().count(), 0);
}
