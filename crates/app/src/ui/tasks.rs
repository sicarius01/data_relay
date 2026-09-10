use super::*;
use csv_agent_core::config::{ScheduleKind, TaskConfig};

impl AgentApp {
    fn select_task(&mut self, index: usize) {
        self.selected_task = Some(index);
        self.task_args = self.draft.task[index].args.join("\n");
        self.history.clear();
        self.load_history(self.draft.task[index].name.clone());
    }

    pub(super) fn load_history(&mut self, name: String) {
        let (reply, receiver) = oneshot::channel();
        self.history_reply = Some(receiver);
        self.command(Command::TaskHistory { name, reply });
    }

    pub(super) fn tasks(&mut self, ui: &mut egui::Ui, snapshot: Option<&StatusSnapshot>) {
        ui.heading("주기 명령 실행");
        ui.label("예약 실행은 콘솔 창을 숨깁니다. ‘예약 실행 사용’은 정해진 주기에 자동으로 실행할지 선택하는 옵션입니다.");
        let mut select = None;
        egui::Grid::new("tasks_table")
            .num_columns(6)
            .striped(true)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for label in [
                    "이름",
                    "프로그램",
                    "주기",
                    "예약 실행",
                    "마지막 실행",
                    "종료 코드",
                ] {
                    ui.strong(label);
                }
                ui.end_row();
                for (index, task) in self.draft.task.iter_mut().enumerate() {
                    if ui
                        .selectable_label(
                            self.selected_task == Some(index),
                            if task.name.is_empty() {
                                "(새 작업)"
                            } else {
                                &task.name
                            },
                        )
                        .clicked()
                    {
                        select = Some(index);
                    }
                    ui.label(&task.program);
                    ui.label(schedule_label(task));
                    ui.checkbox(&mut task.enabled, "").on_hover_text(
                        "설정한 주기에 자동 실행합니다. 예약 실행의 콘솔 창은 숨겨집니다.",
                    );
                    let status = snapshot.and_then(|snapshot| {
                        snapshot
                            .tasks
                            .iter()
                            .find(|status| status.name == task.name)
                    });
                    ui.label(
                        status
                            .and_then(|status| status.last.as_ref())
                            .map(|run| run.started_at.as_str())
                            .unwrap_or("—"),
                    );
                    ui.label(
                        status
                            .and_then(|status| status.last.as_ref())
                            .and_then(|run| run.exit_code)
                            .map(|code| code.to_string())
                            .unwrap_or_else(|| "—".into()),
                    );
                    ui.end_row();
                }
            });
        if let Some(index) = select {
            self.select_task(index);
        }
        ui.horizontal(|ui| {
            if ui.button("작업 추가").clicked() {
                self.draft.task.push(TaskConfig::default());
                self.select_task(self.draft.task.len() - 1);
            }
            if ui
                .add_enabled(
                    self.selected_task.is_some(),
                    egui::Button::new("선택 작업 삭제"),
                )
                .clicked()
            {
                if let Some(index) = self.selected_task.take() {
                    self.draft.task.remove(index);
                    self.history.clear();
                }
            }
        });
        let Some(index) = self
            .selected_task
            .filter(|index| *index < self.draft.task.len())
        else {
            return;
        };
        ui.add_space(12.0);
        ui.separator();
        ui.heading("작업 편집");
        let task = &mut self.draft.task[index];
        let mut test_requested = false;
        let name_running = snapshot.is_some_and(|snapshot| {
            snapshot
                .tasks
                .iter()
                .any(|status| status.name == task.name && status.running)
        });
        let test_available = self.test_task_reply.is_none() && !name_running;
        egui::Grid::new("task_editor")
            .num_columns(2)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                text_row(ui, "이름", &mut task.name);
                ui.label("프로그램");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut task.program).desired_width(430.0));
                    if ui.button("파일 선택").clicked() {
                        if let Some(file) = rfd::FileDialog::new().pick_file() {
                            task.program = file.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                ui.label("인자 · 한 줄에 하나");
                if ui
                    .add(
                        egui::TextEdit::multiline(&mut self.task_args)
                            .desired_rows(3)
                            .desired_width(540.0),
                    )
                    .changed()
                {
                    task.args = if self.task_args.is_empty() {
                        vec![]
                    } else {
                        self.task_args.split('\n').map(str::to_owned).collect()
                    };
                }
                ui.end_row();
                let mut working_dir = task.working_dir.clone().unwrap_or_default();
                ui.label("작업 디렉토리 (선택)");
                if ui
                    .add(egui::TextEdit::singleline(&mut working_dir).desired_width(540.0))
                    .changed()
                {
                    task.working_dir = if working_dir.is_empty() {
                        None
                    } else {
                        Some(working_dir)
                    };
                }
                ui.end_row();
                ui.label("주기");
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt("schedule_kind")
                        .selected_text(match task.schedule {
                            ScheduleKind::Hourly => "매시 정각",
                            ScheduleKind::EveryNMin => "n분마다",
                            ScheduleKind::Daily => "매일",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut task.schedule,
                                ScheduleKind::Hourly,
                                "매시 정각",
                            );
                            ui.selectable_value(
                                &mut task.schedule,
                                ScheduleKind::EveryNMin,
                                "n분마다",
                            );
                            ui.selectable_value(&mut task.schedule, ScheduleKind::Daily, "매일");
                        });
                    match task.schedule {
                        ScheduleKind::EveryNMin => {
                            ui.add(
                                egui::DragValue::new(task.interval_min.get_or_insert(60))
                                    .range(1..=1440)
                                    .suffix("분"),
                            );
                        }
                        ScheduleKind::Daily => {
                            ui.add(
                                egui::TextEdit::singleline(
                                    task.daily_at.get_or_insert_with(|| "09:00".into()),
                                )
                                .desired_width(80.0)
                                .hint_text("HH:MM"),
                            );
                        }
                        _ => {}
                    }
                });
                ui.end_row();
                number_row(ui, "타임아웃 (초)", &mut task.timeout_sec);
                ui.label("실행 옵션");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut task.enabled, "예약 실행 사용").on_hover_text("체크하면 저장된 주기에 자동 실행합니다. 콘솔 창 표시 여부와는 관계없습니다.");
                    ui.add_enabled(task.enabled, egui::Checkbox::new(&mut task.run_on_start, "에이전트 시작 시 1회 실행"));
                    test_requested = ui.add_enabled(test_available,
                        egui::Button::new(egui::RichText::new("테스트 실행 (CMD 창 표시)").strong())
                            .fill(ui.visuals().selection.bg_fill)).clicked();
                });
                ui.end_row();
            });
        let name = task.name.clone();
        let test_config = test_requested.then(|| task.clone());
        ui.label("테스트 실행은 현재 입력한 명령을 저장하지 않고 1회 실행하며, CMD 창에 출력과 종료 결과를 표시합니다. 예약 실행을 꺼도 사용할 수 있습니다.");
        ui.small("예약 실행 옵션의 변경은 저장 및 적용 후 반영됩니다.");
        if let Some(config) = test_config {
            self.test_task_result = None;
            self.test_task_name = Some(config.name.clone());
            let (reply, receiver) = oneshot::channel();
            self.test_task_reply = Some(receiver);
            self.command(Command::TestTask { config, reply });
        }
        let status =
            snapshot.and_then(|snapshot| snapshot.tasks.iter().find(|status| status.name == name));
        if let Some(status) = status {
            ui.label(if status.running {
                "실행 중".into()
            } else {
                format!(
                    "현재 적용된 예약의 다음 실행: {}",
                    status
                        .next_run
                        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_else(|| "예약 실행 꺼짐".into())
                )
            });
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    self.history_reply.is_none(),
                    egui::Button::new("실행 이력 새로고침"),
                )
                .clicked()
            {
                self.load_history(name.clone());
            }
        });
        if let Some(name) = &self.test_task_name {
            ui.colored_label(
                ui.visuals().warn_fg_color,
                format!("테스트 실행 중: {name} · 완료 후 종료 코드와 출력을 표시합니다."),
            );
        } else if name_running {
            ui.label("같은 이름의 작업이 실행 중입니다. 완료 후 테스트할 수 있습니다.");
        }
        if let Some((result_name, result)) = &self.test_task_result {
            if result_name == &name {
                ui.group(|ui| {
                    ui.strong("최근 테스트 실행 결과");
                    match result {
                        Err(error) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, error);
                        }
                        Ok(run) => {
                            let success =
                                run.exit_code == Some(0) && !run.timed_out && !run.skipped;
                            ui.colored_label(
                                if success {
                                    egui::Color32::from_rgb(25, 130, 80)
                                } else {
                                    egui::Color32::LIGHT_RED
                                },
                                format!(
                                    "{} · 종료 코드 {} · {} ms{}{}",
                                    if success {
                                        "완료"
                                    } else {
                                        "실행 결과 확인 필요"
                                    },
                                    run.exit_code
                                        .map(|code| code.to_string())
                                        .unwrap_or_else(|| "없음".into()),
                                    run.duration_ms.unwrap_or(0),
                                    if run.timed_out {
                                        " · 타임아웃"
                                    } else {
                                        ""
                                    },
                                    if run.skipped {
                                        " · 중복 실행 건너뜀"
                                    } else {
                                        ""
                                    }
                                ),
                            );
                            egui::ScrollArea::both()
                                .id_salt("test_task_output")
                                .max_height(150.0)
                                .show(ui, |ui| {
                                    ui.strong("stdout");
                                    ui.monospace(
                                        run.stdout_tail
                                            .as_deref()
                                            .filter(|value| !value.is_empty())
                                            .unwrap_or("(없음)"),
                                    );
                                    ui.strong("stderr");
                                    ui.monospace(
                                        run.stderr_tail
                                            .as_deref()
                                            .filter(|value| !value.is_empty())
                                            .unwrap_or("(없음)"),
                                    );
                                });
                        }
                    }
                });
            }
        }
        for error in self
            .draft
            .validate()
            .into_iter()
            .filter(|error| error.field.starts_with(&format!("task[{index}]")))
        {
            ui.colored_label(egui::Color32::LIGHT_RED, error.to_string());
        }
        ui.add_space(12.0);
        ui.heading("최근 실행 이력 (최대 20건)");
        if self.history_reply.is_some() {
            ui.label("조회 중…");
        }
        if self.history.is_empty() && self.history_reply.is_none() {
            ui.label("실행 이력이 없습니다.");
        }
        for run in &self.history {
            let label = format!(
                "{} · 종료 {} · {} ms{}{}",
                run.started_at,
                run.exit_code
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "—".into()),
                run.duration_ms.unwrap_or(0),
                if run.timed_out {
                    " · 타임아웃"
                } else {
                    ""
                },
                if run.skipped {
                    " · 중복 실행 건너뜀"
                } else {
                    ""
                }
            );
            egui::CollapsingHeader::new(label)
                .id_salt(run.id)
                .show(ui, |ui| {
                    egui::ScrollArea::both()
                        .id_salt(("task_output", run.id))
                        .max_height(220.0)
                        .show(ui, |ui| {
                            ui.strong("stdout");
                            ui.monospace(run.stdout_tail.as_deref().unwrap_or("(없음)"));
                            ui.strong("stderr");
                            ui.monospace(run.stderr_tail.as_deref().unwrap_or("(없음)"));
                        });
                });
        }
    }
}

fn schedule_label(task: &TaskConfig) -> String {
    match task.schedule {
        ScheduleKind::Hourly => "매시 정각".into(),
        ScheduleKind::EveryNMin => format!("{}분마다", task.interval_min.unwrap_or(0)),
        ScheduleKind::Daily => format!("매일 {}", task.daily_at.as_deref().unwrap_or("—")),
    }
}
