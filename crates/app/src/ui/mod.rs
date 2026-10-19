mod global;
mod status;
mod tasks;
mod watch;

use crate::{
    events::{EventSink, UiEvent},
    platform,
    tray::Tray,
};
use data_relay_core::{
    bus::{Command, StatusSnapshot},
    config::{self, Config, ConfigLoad},
    paths::AppPaths,
    runtime::AgentHandle,
    state::TaskRun,
};
use std::{path::PathBuf, sync::Arc};
use tokio::sync::oneshot;

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Global,
    Tasks,
    Watch,
}
enum Confirm {
    Exit,
    Reset { key: String, path: String },
    ResetAll { keys: Vec<String> },
    Reload,
    Error(String),
}

pub struct AgentApp {
    handle: Option<AgentHandle>,
    draft: Config,
    applied: Config,
    paths: AppPaths,
    tab: Tab,
    visible: bool,
    initial_visibility_pending: bool,
    sink: EventSink,
    tray: Tray,
    _instance: platform::SingleInstance,
    #[cfg(windows)]
    _power: Option<platform::PowerNotifications>,
    confirm: Option<Confirm>,
    focus_cancel: bool,
    shutting_down: bool,
    allow_close: bool,
    notice: String,
    selected_task: Option<usize>,
    selected_watch: Option<usize>,
    selected_file: Option<String>,
    task_args: String,
    watch_columns: String,
    connection_probe: status::ConnectionProbe,
    preview_reply: Option<oneshot::Receiver<Result<Vec<PathBuf>, String>>>,
    preview: Option<Result<Vec<PathBuf>, String>>,
    history_reply: Option<oneshot::Receiver<Vec<TaskRun>>>,
    history: Vec<TaskRun>,
    test_task_reply: Option<oneshot::Receiver<Result<TaskRun, String>>>,
    test_task_name: Option<String>,
    test_task_result: Option<(String, Result<TaskRun, String>)>,
}

impl AgentApp {
    pub fn new(
        handle: AgentHandle,
        config: Config,
        paths: AppPaths,
        visible: bool,
        sink: EventSink,
        instance: platform::SingleInstance,
    ) -> anyhow::Result<Self> {
        let tray = Tray::new(sink.clone())?;
        let mut notice = String::new();
        #[cfg(windows)]
        let power = match platform::PowerNotifications::register(handle.cmd_tx.clone()) {
            Ok(power) => Some(power),
            Err(error) => {
                notice = format!("절전 복귀 알림 등록 실패: {error}");
                None
            }
        };
        if let Err(error) = data_relay_core::win::set_autostart(config.agent.autostart) {
            notice = format!("자동 시작 등록 실패: {error}");
        }
        Ok(Self {
            handle: Some(handle),
            draft: config.clone(),
            applied: config,
            paths,
            tab: Tab::Global,
            visible,
            initial_visibility_pending: true,
            sink,
            tray,
            _instance: instance,
            #[cfg(windows)]
            _power: power,
            confirm: None,
            focus_cancel: false,
            shutting_down: false,
            allow_close: false,
            notice,
            selected_task: None,
            selected_watch: None,
            selected_file: None,
            task_args: String::new(),
            watch_columns: String::new(),
            connection_probe: status::ConnectionProbe::default(),
            preview_reply: None,
            preview: None,
            history_reply: None,
            history: vec![],
            test_task_reply: None,
            test_task_name: None,
            test_task_result: None,
        })
    }

    fn command(&mut self, command: Command) {
        if let Some(handle) = &self.handle {
            if let Err(error) = handle.send(command) {
                self.confirm = Some(Confirm::Error(error));
            }
        }
    }

    fn show(&mut self, context: &egui::Context) {
        self.visible = true;
        context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
        context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        context.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn save(&mut self) {
        let errors = self.draft.validate();
        if !errors.is_empty() {
            self.confirm = Some(Confirm::Error(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ));
            return;
        }
        if let Err(error) = config::save(&self.paths.config, &self.draft) {
            self.confirm = Some(Confirm::Error(format!(
                "설정을 저장하지 못했습니다: {error:#}"
            )));
            return;
        }
        let needs_restart = self.needs_restart(&self.draft);
        if let Some(handle) = &self.handle {
            if let Err(error) = handle.send(Command::ReloadConfig(self.draft.clone())) {
                self.confirm = Some(Confirm::Error(format!(
                    "설정 파일은 저장했지만 실행 중인 에이전트에 적용하지 못했습니다: {error}"
                )));
                return;
            }
        }
        self.applied = self.draft.clone();
        self.notice = if needs_restart {
            "저장했습니다. 스풀 경로 / 로그 경로 / 로그 보관 수 변경은 재시작 후 적용됩니다."
        } else {
            "저장하고 실행 중인 에이전트에 적용했습니다."
        }
        .into();
    }

    fn needs_restart(&self, next: &Config) -> bool {
        self.applied.agent.spool_dir != next.agent.spool_dir
            || self.applied.log.dir != next.log.dir
            || self.applied.log.max_files != next.log.max_files
    }

    /// Re-reads config.toml (edited outside the app) and applies it like a save.
    fn reload_from_file(&mut self) {
        let path = &self.paths.config;
        // load() returns defaults for a missing file; never apply those silently.
        if !path.exists() {
            self.confirm = Some(Confirm::Error(format!(
                "설정 파일이 없습니다. 현재 설정을 유지합니다.\n{}",
                path.display()
            )));
            return;
        }
        let config = match config::load(path) {
            Ok(ConfigLoad::Loaded { config, errors }) if errors.is_empty() => config,
            Ok(ConfigLoad::Loaded { config, errors }) => {
                self.draft = config;
                self.selected_task = None;
                self.selected_watch = None;
                self.confirm = Some(Confirm::Error(format!(
                    "파일을 불러왔지만 설정 오류가 있어 적용하지 않았습니다. 화면에서 고친 뒤 저장 및 적용을 누르세요.\n{}",
                    errors
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join("\n")
                )));
                return;
            }
            Ok(ConfigLoad::Broken { err }) => {
                self.confirm = Some(Confirm::Error(format!(
                    "설정 파일 형식이 잘못되어 현재 설정을 유지합니다.\n{err}"
                )));
                return;
            }
            Err(error) => {
                self.confirm = Some(Confirm::Error(format!(
                    "설정 파일을 읽지 못해 현재 설정을 유지합니다: {error:#}"
                )));
                return;
            }
        };
        let needs_restart = self.needs_restart(&config);
        if let Some(handle) = &self.handle {
            if let Err(error) = handle.send(Command::ReloadConfig(config.clone())) {
                self.confirm = Some(Confirm::Error(format!(
                    "설정 파일을 읽었지만 실행 중인 에이전트에 적용하지 못했습니다: {error}"
                )));
                return;
            }
        }
        self.applied = config.clone();
        self.draft = config;
        self.selected_task = None;
        self.selected_watch = None;
        self.notice = if needs_restart {
            "파일에서 다시 읽어 적용했습니다. 스풀 경로 / 로그 경로 / 로그 보관 수 변경은 재시작 후 적용됩니다."
        } else {
            "파일에서 다시 읽어 실행 중인 에이전트에 적용했습니다."
        }
        .into();
    }

    fn receive_replies(&mut self) {
        fn receive<T>(pending: &mut Option<oneshot::Receiver<T>>) -> Option<Result<T, String>> {
            let receiver = pending.as_mut()?;
            match receiver.try_recv() {
                Ok(value) => {
                    *pending = None;
                    Some(Ok(value))
                }
                Err(oneshot::error::TryRecvError::Empty) => None,
                Err(oneshot::error::TryRecvError::Closed) => {
                    *pending = None;
                    Some(Err("요청 처리 중 연결이 닫혔습니다.".into()))
                }
            }
        }
        // Each reply is examined once only on a real event-driven UI frame.
        self.connection_probe.receive(&self.draft.server.unc_base);
        if let Some(result) = receive(&mut self.preview_reply) {
            self.preview = Some(result.and_then(|r| r));
        }
        if let Some(result) = receive(&mut self.history_reply) {
            match result {
                Ok(history) => self.history = history,
                Err(error) => self.notice = error,
            }
        }
        if let Some(result) = receive(&mut self.test_task_reply) {
            if let Some(name) = self.test_task_name.take() {
                self.test_task_result = Some((name.clone(), result.and_then(|result| result)));
                if self
                    .selected_task
                    .and_then(|index| self.draft.task.get(index))
                    .is_some_and(|task| task.name == name)
                {
                    self.load_history(name);
                }
            }
        }
    }

    fn modal(&mut self, context: &egui::Context) {
        let Some(confirm) = self.confirm.take() else {
            return;
        };
        let mut close = false;
        let mut execute = false;
        let response = egui::Modal::new(egui::Id::new("confirmation")).show(context, |ui| {
            ui.set_width(440.0);
            match &confirm {
                Confirm::Exit => {
                    ui.heading("data_relay를 종료할까요?");
                    ui.add_space(12.0);
                    ui.label("종료하면 데이터 수집이 중단됩니다.");
                    if !self.applied.ui.exit_confirm_message.is_empty() {
                        ui.add_space(8.0);
                        ui.label(&self.applied.ui.exit_confirm_message);
                    }
                    if self.draft != self.applied {
                        ui.colored_label(
                            ui.visuals().warn_fg_color,
                            "저장하지 않은 설정 변경은 사라집니다.",
                        );
                    }
                }
                Confirm::Reset { path, .. } => {
                    ui.heading("파일을 처음부터 다시 업로드할까요?");
                    ui.label(path);
                    ui.label("저장된 offset이 0으로 바뀌며 기존 데이터가 중복 전송될 수 있습니다.");
                }
                Confirm::ResetAll { keys } => {
                    ui.heading("모든 파일을 처음부터 다시 업로드할까요?");
                    ui.label(format!(
                        "감시 중인 파일 {}개의 offset이 0으로 바뀝니다.",
                        keys.len()
                    ));
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "이미 전송한 데이터가 모두 다시 전송됩니다. 파일이 크면 시간이 걸립니다.",
                    );
                }
                Confirm::Reload => {
                    ui.heading("설정 파일을 다시 읽어 적용할까요?");
                    ui.label(self.paths.config.display().to_string());
                    ui.colored_label(
                        ui.visuals().warn_fg_color,
                        "화면에서 저장하지 않은 설정 변경은 사라집니다.",
                    );
                }
                Confirm::Error(error) => {
                    ui.heading("확인 필요");
                    ui.label(error);
                }
            }
            ui.add_space(16.0);
            ui.horizontal(|ui| {
                if !matches!(&confirm, Confirm::Error(_))
                    && ui
                        .button(match &confirm {
                            Confirm::Exit => "종료",
                            Confirm::Reload => "다시 읽기",
                            Confirm::ResetAll { .. } => "전체 리셋",
                            _ => "offset 리셋",
                        })
                        .clicked()
                {
                    execute = true;
                }
                let cancel = ui.button(if matches!(&confirm, Confirm::Error(_)) {
                    "닫기"
                } else {
                    "취소"
                });
                if self.focus_cancel {
                    cancel.request_focus();
                    self.focus_cancel = false;
                }
                close = cancel.clicked();
            });
        });
        if execute {
            match confirm {
                Confirm::Exit => {
                    self.shutting_down = true;
                    self.notice = "진행 중인 작업을 마무리하고 종료하는 중입니다…".into();
                    if let Some(handle) = self.handle.take() {
                        let sink = self.sink.clone();
                        std::thread::spawn(move || {
                            sink.send(UiEvent::ShutdownFinished(handle.shutdown()));
                        });
                    }
                }
                Confirm::Reset { key, .. } => self.command(Command::ResetOffset(key)),
                Confirm::ResetAll { keys } => {
                    let count = keys.len();
                    for key in keys {
                        self.command(Command::ResetOffset(key));
                    }
                    self.notice = format!("파일 {count}개를 처음부터 다시 업로드합니다.");
                }
                Confirm::Reload => self.reload_from_file(),
                Confirm::Error(_) => {}
            }
        } else if !close && !response.should_close() {
            self.confirm = Some(confirm);
        }
    }

    fn logs(&mut self) {
        if let Err(error) = platform::open_logs(std::path::Path::new(&self.applied.log.dir)) {
            self.confirm = Some(Confirm::Error(format!(
                "로그 폴더를 열지 못했습니다: {error}"
            )));
        }
    }
}

impl eframe::App for AgentApp {
    fn update(&mut self, context: &egui::Context, _: &mut eframe::Frame) {
        if self.initial_visibility_pending {
            self.initial_visibility_pending = false;
            // eframe 0.31 shows the root after its first frame regardless of
            // NativeOptions. Override that hook exactly once. Repeating a hide
            // command causes WINDOWPOS events and would create an idle loop.
            context.send_viewport_cmd(egui::ViewportCommand::Visible(self.visible));
        }
        for event in self.sink.drain() {
            tracing::debug!(?event, "handling UI event");
            match event {
                UiEvent::Show => self.show(context),
                UiEvent::Flush => self.command(Command::FlushNow),
                UiEvent::Logs => self.logs(),
                UiEvent::Exit => {
                    self.show(context);
                    if !self.shutting_down {
                        self.confirm = Some(Confirm::Exit);
                        self.focus_cancel = true;
                    }
                }
                UiEvent::ShutdownFinished(result) => {
                    if let Err(error) = result {
                        tracing::error!(%error, "agent shutdown failed");
                    }
                    self.allow_close = true;
                    context.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }
        if context.input(|input| input.viewport().close_requested()) && !self.allow_close {
            context.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            context.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.visible = false;
        }
        self.receive_replies();
        let snapshot: Option<Arc<StatusSnapshot>> = self.handle.as_ref().map(AgentHandle::snapshot);
        if let Some(snapshot) = &snapshot {
            self.tray.sync(snapshot);
        }
        if !self.visible {
            return;
        }
        egui::TopBottomPanel::top("navigation").show(context, |ui| {
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.heading("data_relay");
                if let Some(snapshot) = &snapshot {
                    let pending = status::connection_settings_pending(&self.draft, &self.applied);
                    let (label, color) = if pending {
                        ("설정 저장 필요", ui.visuals().warn_fg_color)
                    } else { match snapshot.tray {
                        data_relay_core::bus::TrayState::Ok => {
                            ("정상", egui::Color32::from_rgb(35, 169, 114))
                        }
                        data_relay_core::bus::TrayState::Pending => {
                            ("전송 대기", egui::Color32::from_rgb(220, 162, 38))
                        }
                        data_relay_core::bus::TrayState::Error => {
                            (if !snapshot.config_errors.is_empty() { "적용된 설정 확인 필요" }
                                else if !snapshot.watch_errors.is_empty() { "감시 경로 확인 필요" }
                                else { "전송 재시도 중단" }, egui::Color32::from_rgb(230, 89, 95))
                        }
                    }};
                    ui.colored_label(color, label);
                }
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.tab, Tab::Global, "전역 설정");
                ui.selectable_value(&mut self.tab, Tab::Tasks, "주기 명령 실행");
                ui.selectable_value(&mut self.tab, Tab::Watch, "파일 감시 및 전송");
            });
            if let Some(snapshot) = &snapshot {
                if status::connection_settings_pending(&self.draft, &self.applied) {
                    ui.colored_label(ui.visuals().warn_fg_color, "입력한 장비/서버 설정은 아직 적용되지 않았습니다. 저장 및 적용을 눌러 수집에 사용하세요.");
                }
                for error in &snapshot.config_errors {
                    if !status::is_unapplied_connection_error(error, &self.draft, &self.applied) {
                        ui.colored_label(egui::Color32::LIGHT_RED, format!("현재 적용된 설정: {error}"));
                    }
                }
                for error in &snapshot.watch_errors {
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("감시 경로 {}: {}", error.pattern, error.reason));
                }
                if matches!(snapshot.uploader, data_relay_core::bus::UploaderState::Suspended) {
                    let reason = snapshot.last_error.as_ref().map(|error| error.message.as_str()).unwrap_or("전송이 반복 실패했습니다.");
                    ui.colored_label(egui::Color32::LIGHT_RED, format!("전송 재시도 중단: {reason}"));
                }
                if let Some(error) = snapshot
                    .last_error
                    .as_ref()
                    .filter(|error| error.component == "config")
                {
                    ui.colored_label(egui::Color32::LIGHT_RED, &error.message);
                }
            }
            ui.add_space(6.0);
        });
        egui::TopBottomPanel::bottom("save_bar").show(context, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let dirty = self.draft != self.applied;
                if ui
                    .add_enabled(!self.shutting_down, egui::Button::new("저장 및 적용"))
                    .clicked()
                {
                    self.save();
                }
                if ui
                    .add_enabled(dirty && !self.shutting_down, egui::Button::new("변경 취소"))
                    .clicked()
                {
                    self.draft = self.applied.clone();
                    self.selected_task = None;
                    self.selected_watch = None;
                    self.notice = "저장된 설정으로 되돌렸습니다.".into();
                }
                if ui
                    .add_enabled(!self.shutting_down, egui::Button::new("파일에서 다시 읽기"))
                    .on_hover_text("config.toml을 직접 고친 뒤 눌러 다시 읽고 적용합니다.")
                    .clicked()
                {
                    if dirty {
                        self.confirm = Some(Confirm::Reload);
                        self.focus_cancel = true;
                    } else {
                        self.reload_from_file();
                    }
                }
                if dirty {
                    ui.label("저장하지 않은 변경");
                }
                ui.label(&self.notice);
            });
            ui.horizontal(|ui| {
                ui.small(format!(
                    "설정: {} · 창을 닫으면 트레이에서 계속 실행됩니다.",
                    self.paths.config.display()
                ));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.label(egui::RichText::new("made by PHS").small().weak());
                });
            });
            ui.add_space(6.0);
        });
        egui::CentralPanel::default().show(context, |ui| {
            ui.add_enabled_ui(!self.shutting_down, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| match self.tab {
                    Tab::Global => self.global(ui),
                    Tab::Tasks => self.tasks(ui, snapshot.as_deref()),
                    Tab::Watch => self.watch(ui, snapshot.as_deref()),
                });
            });
        });
        self.modal(context);
    }
}

impl Drop for AgentApp {
    fn drop(&mut self) {
        self.sink.detach();
    }
}

fn text_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(540.0));
    ui.end_row();
}
fn number_row<T: egui::emath::Numeric>(ui: &mut egui::Ui, label: &str, value: &mut T) {
    ui.label(label);
    ui.add(egui::DragValue::new(value));
    ui.end_row();
}
fn bytes(value: u64) -> String {
    if value >= 1 << 30 {
        format!("{:.2} GiB", value as f64 / (1u64 << 30) as f64)
    } else if value >= 1 << 20 {
        format!("{:.2} MiB", value as f64 / (1u64 << 20) as f64)
    } else if value >= 1 << 10 {
        format!("{:.1} KiB", value as f64 / (1u64 << 10) as f64)
    } else {
        format!("{value} B")
    }
}
