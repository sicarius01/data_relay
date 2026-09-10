#![cfg_attr(target_os = "windows", windows_subsystem = "windows")]

mod events;
mod platform;
mod tray;
mod ui;

use csv_agent_core::{
    config::{self, Config, ConfigLoad},
    paths::AppPaths,
};
use std::{path::PathBuf, sync::Arc};

fn main() -> std::process::ExitCode {
    // The output viewer is a child console, not a second tray application.
    // Handle it before instance locks, config loading, and autostart changes.
    if std::env::args().nth(1).as_deref() == Some("--task-console-viewer") {
        return match csv_agent_core::win::console_viewer::run() {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("테스트 출력 창 오류: {error:#}");
                std::process::ExitCode::FAILURE
            }
        };
    }
    if let Err(error) = run() {
        let message = format!("CSV 수집 에이전트를 시작하지 못했습니다.\n\n{error:#}");
        eprintln!("{message}");
        rfd::MessageDialog::new()
            .set_title("CSV 수집 에이전트")
            .set_description(message)
            .set_level(rfd::MessageLevel::Error)
            .show();
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn run() -> anyhow::Result<()> {
    let sink = events::EventSink::default();
    let Some(instance) = platform::SingleInstance::acquire(sink.clone())? else {
        return Ok(());
    };
    let mut args = std::env::args().skip(1);
    let mut base = None;
    let mut show = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data-dir" => {
                base = Some(PathBuf::from(args.next().ok_or_else(|| {
                    anyhow::anyhow!("--data-dir requires a directory")
                })?))
            }
            "--show" => show = true,
            "--hidden" => {}
            _ => anyhow::bail!("알 수 없는 인자: {arg}"),
        }
    }
    let custom_base = base.is_some();
    let paths = match base {
        Some(base) => AppPaths::from_base(base),
        None => AppPaths::resolve()?,
    };
    if custom_base && !paths.config.exists() {
        let mut config = Config::default();
        config.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
        config.log.dir = paths.logs.to_string_lossy().into_owned();
        config::save(&paths.config, &config)?;
    }
    let (config, startup_error) = match config::load(&paths.config)? {
        ConfigLoad::Loaded { config, .. } => (config, None),
        ConfigLoad::Broken { err } => {
            let mut fallback = Config::default();
            if custom_base {
                fallback.agent.spool_dir = paths.spool.to_string_lossy().into_owned();
                fallback.log.dir = paths.logs.to_string_lossy().into_owned();
            }
            (
                fallback,
                Some(format!("설정 파일을 읽지 못했습니다: {err}")),
            )
        }
    };
    let mut log_config = config.log.clone();
    if !["trace", "debug", "info", "warn", "error"].contains(&log_config.level.as_str()) {
        log_config.level = "info".into();
    }
    let _logging = csv_agent_core::logging::init(&log_config)?;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        "starting tray application"
    );
    let visible = show || startup_error.is_some() || !config.validate().is_empty();
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("CSV 수집 에이전트")
            .with_inner_size([1080.0, 800.0])
            .with_min_inner_size([800.0, 600.0])
            .with_visible(visible),
        ..Default::default()
    };
    eframe::run_native(
        "CSV 수집 에이전트",
        native_options,
        Box::new(move |cc| {
            sink.attach(cc.egui_ctx.clone());
            #[cfg(windows)]
            {
                use raw_window_handle::{HasWindowHandle, RawWindowHandle};
                if let Ok(handle) = cc.window_handle() {
                    if let RawWindowHandle::Win32(handle) = handle.as_raw() {
                        sink.attach_window(handle.hwnd.get() as usize);
                    }
                }
            }
            install_korean_font(&cc.egui_ctx);
            let wake = sink.clone();
            let handle = csv_agent_core::runtime::start(
                config.clone(),
                paths.clone(),
                Arc::new(move || wake.wake()),
            )?;
            if let Some(error) = startup_error {
                let _ = handle.send(csv_agent_core::bus::Command::ConfigError(error));
            }
            Ok(Box::new(ui::AgentApp::new(
                handle, config, paths, visible, sink, instance,
            )?))
        }),
    )
    .map_err(|error| anyhow::anyhow!(error.to_string()))
}

fn install_korean_font(context: &egui::Context) {
    #[cfg(windows)]
    {
        let windows = std::env::var_os("WINDIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("C:/Windows"));
        if let Ok(data) = std::fs::read(windows.join("Fonts/malgun.ttf")) {
            let mut fonts = egui::FontDefinitions::default();
            fonts
                .font_data
                .insert("korean".into(), egui::FontData::from_owned(data).into());
            for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
                fonts
                    .families
                    .entry(family)
                    .or_default()
                    .push("korean".into());
            }
            context.set_fonts(fonts);
        }
    }
    context.style_mut(|style| {
        style.animation_time = 0.0;
    });
}
