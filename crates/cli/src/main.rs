use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use csv_agent_core::{
    config::{self, Config, ConfigLoad},
    paths::AppPaths,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

#[derive(Parser)]
#[command(version, about = "CSV agent headless diagnostics and collection")]
struct Args {
    /// Directory containing config.toml and state.db.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,
    /// Override the configuration file location.
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Permit an absolute local destination for integration testing.
    #[arg(long, global = true)]
    allow_local_target: bool,
    #[command(subcommand)]
    command: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Monitor and collect until Ctrl+C (or the requested duration).
    Run {
        /// Log debounced file events without collecting or running tasks.
        #[arg(long)]
        dry_run: bool,
        /// Stop cleanly after this many seconds, useful for smoke tests.
        #[arg(long)]
        duration_sec: Option<u64>,
    },
    /// Validate configuration without starting workers.
    CheckConfig,
    /// List existing files matching an absolute glob pattern.
    Scan {
        #[arg(long)]
        pattern: String,
    },
    /// Decode the source name, byte range, and header kind.
    ChunkName {
        #[arg(long)]
        parse: String,
    },
    /// Write and delete a uniquely named probe in the destination.
    TestUnc {
        #[arg(long)]
        base: Option<String>,
    },
}

fn main() -> std::process::ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--task-console-viewer"))
    {
        return match csv_agent_core::win::console_viewer::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("test console: {error:#}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    match execute(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn execute(args: Args) -> Result<()> {
    match &args.command {
        Action::Scan { pattern } => {
            for path in csv_agent_core::watch::scan(pattern)? {
                println!("{}", path.display());
            }
            return Ok(());
        }
        Action::ChunkName { parse } => {
            let name =
                csv_agent_core::paths::parse_chunk_name(parse).context("invalid chunk filename")?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "name_seg": name.name_seg, "start": name.start, "end": name.end,
                    "header_kind": name.kind.as_str()
                }))?
            );
            return Ok(());
        }
        Action::TestUnc { base: Some(base) } => {
            check_target(base, args.allow_local_target)?;
            csv_agent_core::upload::test_unc(base)?;
            println!("Connection test succeeded: {base}");
            return Ok(());
        }
        _ => {}
    }

    let custom_data = args.data_dir.is_some();
    let mut paths = match args.data_dir {
        Some(base) => AppPaths::from_base(std::path::absolute(base)?),
        None => AppPaths::resolve()?,
    };
    if let Some(path) = args.config {
        paths.config = std::path::absolute(path)?;
    }
    if !paths.config.exists() {
        let mut defaults = Config::default();
        if custom_data {
            defaults.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
            defaults.log.dir = paths.logs.to_string_lossy().into_owned();
        }
        config::save(&paths.config, &defaults)?;
        eprintln!("Created configuration: {}", paths.config.display());
    }
    let config = match config::load(&paths.config)? {
        ConfigLoad::Loaded { config, .. } => config,
        ConfigLoad::Broken { err } => bail!("configuration is invalid (original preserved): {err}"),
    };
    if matches!(args.command, Action::TestUnc { .. }) {
        check_target(&config.server.unc_base, args.allow_local_target)?;
        csv_agent_core::upload::test_unc(&config.server.unc_base)?;
        println!("Connection test succeeded: {}", config.server.unc_base);
        return Ok(());
    }
    let errors: Vec<_> = config
        .validate()
        .into_iter()
        .filter(|error| {
            !(args.allow_local_target
                && error.field == "server.unc_base"
                && PathBuf::from(&config.server.unc_base).is_absolute())
        })
        .collect();
    if matches!(args.command, Action::CheckConfig) {
        if !errors.is_empty() {
            for error in &errors {
                eprintln!("{error}");
            }
            bail!("{} configuration error(s)", errors.len());
        }
        println!("Configuration valid: {}", paths.config.display());
        return Ok(());
    }
    // Equipment/server fields can be incomplete on first deployment. Collection
    // still runs into the local spool while transfer stays gated.
    let structural_errors: Vec<_> = errors
        .iter()
        .filter(|error| !error.field.starts_with("equip") && error.field != "server.unc_base")
        .collect();
    if !structural_errors.is_empty() {
        for error in structural_errors {
            eprintln!("{error}");
        }
        bail!("fix configuration before starting collection");
    }
    if !args.allow_local_target
        && PathBuf::from(&config.server.unc_base).is_absolute()
        && !config::valid_unc(&config.server.unc_base)
    {
        bail!("local destination requires --allow-local-target");
    }
    for error in &errors {
        eprintln!("warning: {error}");
    }
    let _logging = csv_agent_core::logging::init(&config.log)?;
    let Action::Run {
        dry_run,
        duration_sec,
    } = args.command
    else {
        unreachable!()
    };
    let handle = if dry_run {
        csv_agent_core::runtime::start_dry_run(config, paths, Arc::new(|| {}))?
    } else {
        csv_agent_core::runtime::start(config, paths, Arc::new(|| {}))?
    };
    println!(
        "Agent started{}. Press Ctrl+C to stop.",
        if dry_run { " (dry run)" } else { "" }
    );
    let wait_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let wait_result: Result<()> = wait_runtime.block_on(async {
        if let Some(seconds) = duration_sec {
            tokio::select! {
                signal = tokio::signal::ctrl_c() => signal.context("install Ctrl+C handler")?,
                _ = tokio::time::sleep(Duration::from_secs(seconds)) => {}
            }
        } else {
            tokio::signal::ctrl_c()
                .await
                .context("install Ctrl+C handler")?;
        }
        Ok(())
    });
    let final_status = handle.status_rx.clone();
    handle.shutdown().map_err(anyhow::Error::msg)?;
    wait_result?;
    let status = final_status.borrow().clone();
    println!(
        "Stopped. Files: {}, pending chunks: {}, bytes sent today: {}",
        status.files.len(),
        status.spool_pending,
        status.bytes_sent_today
    );
    Ok(())
}

fn check_target(base: &str, allow_local: bool) -> Result<()> {
    if config::valid_unc(base) || (allow_local && PathBuf::from(base).is_absolute()) {
        Ok(())
    } else {
        bail!("expected \\\\host\\share; local test destinations require --allow-local-target")
    }
}
