use super::*;
use data_relay_core::{
    bus::UploaderState,
    config::{HeaderMode, WatchConfig},
};

impl AgentApp {
    pub(super) fn watch(&mut self, ui: &mut egui::Ui, snapshot: Option<&StatusSnapshot>) {
        ui.heading("감시 패턴");
        ui.label("절대경로 glob 패턴을 입력하세요. 여러 패턴이 매칭되면 위에서 첫 번째 패턴을 적용합니다.");
        let mut select = None;
        egui::Grid::new("watch_list")
            .num_columns(4)
            .striped(true)
            .spacing([16.0, 8.0])
            .show(ui, |ui| {
                for label in ["패턴", "헤더 모드", "구분자", "활성화"] {
                    ui.strong(label);
                }
                ui.end_row();
                for (index, watch) in self.draft.watch.iter_mut().enumerate() {
                    if ui
                        .selectable_label(
                            self.selected_watch == Some(index),
                            if watch.pattern.is_empty() {
                                "(새 패턴)"
                            } else {
                                &watch.pattern
                            },
                        )
                        .clicked()
                    {
                        select = Some(index);
                    }
                    ui.label(format!("{:?}", watch.header).to_lowercase());
                    ui.label(watch.delimiter.replace('\t', "\\t"));
                    ui.checkbox(&mut watch.enabled, "");
                    ui.end_row();
                }
            });
        if let Some(index) = select {
            self.selected_watch = Some(index);
            self.watch_columns = self.draft.watch[index]
                .columns
                .clone()
                .unwrap_or_default()
                .join(",");
            self.preview = None;
            self.preview_reply = None;
        }
        ui.horizontal(|ui| {
            if ui.button("패턴 추가").clicked() {
                self.draft.watch.push(WatchConfig::default());
                self.selected_watch = Some(self.draft.watch.len() - 1);
                self.watch_columns.clear();
                self.preview = None;
            }
            if ui
                .add_enabled(
                    self.selected_watch.is_some(),
                    egui::Button::new("선택 패턴 삭제"),
                )
                .clicked()
            {
                if let Some(index) = self.selected_watch.take() {
                    self.draft.watch.remove(index);
                    self.preview = None;
                }
            }
            if ui
                .add_enabled(
                    self.selected_watch.is_some_and(|i| i > 0),
                    egui::Button::new("위로"),
                )
                .clicked()
            {
                if let Some(index) = self.selected_watch {
                    self.draft.watch.swap(index, index - 1);
                    self.selected_watch = Some(index - 1);
                }
            }
            if ui
                .add_enabled(
                    self.selected_watch
                        .is_some_and(|i| i + 1 < self.draft.watch.len()),
                    egui::Button::new("아래로"),
                )
                .clicked()
            {
                if let Some(index) = self.selected_watch {
                    self.draft.watch.swap(index, index + 1);
                    self.selected_watch = Some(index + 1);
                }
            }
        });
        if let Some(index) = self
            .selected_watch
            .filter(|index| *index < self.draft.watch.len())
        {
            ui.add_space(10.0);
            let watch = &mut self.draft.watch[index];
            egui::Grid::new("watch_edit")
                .num_columns(2)
                .spacing([16.0, 8.0])
                .show(ui, |ui| {
                    text_row(ui, "glob 패턴", &mut watch.pattern);
                    ui.label("헤더 모드");
                    egui::ComboBox::from_id_salt("header_mode")
                        .selected_text(format!("{:?}", watch.header).to_lowercase())
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut watch.header,
                                HeaderMode::Auto,
                                "auto · 첫 줄이 헤더",
                            );
                            ui.selectable_value(
                                &mut watch.header,
                                HeaderMode::None,
                                "none · 헤더 없음, c0,c1,… 생성",
                            );
                            ui.selectable_value(
                                &mut watch.header,
                                HeaderMode::Fixed,
                                "fixed · 컬럼 직접 지정",
                            );
                        });
                    ui.end_row();
                    text_row(ui, "구분자 (탭: \\t)", &mut watch.delimiter);
                    ui.label("컬럼명 · 쉼표로 구분");
                    if ui
                        .add_enabled(
                            watch.header == HeaderMode::Fixed,
                            egui::TextEdit::singleline(&mut self.watch_columns)
                                .desired_width(540.0),
                        )
                        .changed()
                    {
                        watch.columns = Some(
                            self.watch_columns
                                .split(',')
                                .map(|column| column.trim().to_owned())
                                .collect(),
                        );
                    }
                    ui.end_row();
                });
            let pattern = watch.pattern.clone();
            if ui
                .add_enabled(
                    self.preview_reply.is_none(),
                    egui::Button::new("매칭 미리보기"),
                )
                .clicked()
            {
                let (reply, receiver) = oneshot::channel();
                self.preview_reply = Some(receiver);
                self.preview = None;
                self.command(Command::PreviewPattern { pattern, reply });
            }
            if self.preview_reply.is_some() {
                ui.label("파일을 찾는 중…");
            }
            if let Some(preview) = &self.preview {
                match preview {
                    Err(error) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                    Ok(files) => {
                        ui.label(format!(
                            "매칭 파일 {}건{}",
                            files.len(),
                            if files.len() >= 500 {
                                " (최대 500건 표시)"
                            } else {
                                ""
                            }
                        ));
                        let compiled: Vec<_> = self
                            .draft
                            .watch
                            .iter()
                            .filter(|watch| watch.enabled)
                            .filter_map(|watch| {
                                data_relay_core::paths::CompiledPattern::new(watch).ok()
                            })
                            .collect();
                        egui::ScrollArea::vertical()
                            .id_salt("preview_files")
                            .max_height(160.0)
                            .show(ui, |ui| {
                                for file in files.iter().take(500) {
                                    ui.horizontal(|ui| {
                                        ui.monospace(file.display().to_string());
                                        if compiled
                                            .iter()
                                            .filter(|pattern| pattern.matches(file))
                                            .count()
                                            > 1
                                        {
                                            ui.colored_label(
                                                ui.visuals().warn_fg_color,
                                                "중복 매칭: 첫 패턴 사용",
                                            );
                                        }
                                    });
                                }
                            });
                    }
                }
            }
            for error in self
                .draft
                .validate()
                .into_iter()
                .filter(|error| error.field.starts_with(&format!("watch[{index}]")))
            {
                ui.colored_label(egui::Color32::LIGHT_RED, error.to_string());
            }
        }
        ui.add_space(12.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("디바운스");
            ui.add(egui::DragValue::new(&mut self.draft.agent.debounce_sec).suffix("초"));
            ui.label("최대 대기");
            ui.add(egui::DragValue::new(&mut self.draft.agent.max_wait_sec).suffix("초"));
        });
        let Some(snapshot) = snapshot else {
            return;
        };
        ui.heading("전송 현황");
        ui.horizontal(|ui| {
            ui.label(format!(
                "대기 {}건 ({})",
                snapshot.spool_pending,
                bytes(snapshot.spool_bytes)
            ));
            ui.separator();
            ui.label(format!("오늘 전송 {}", bytes(snapshot.bytes_sent_today)));
            if ui.button("지금 전송").clicked() {
                self.command(Command::FlushNow);
            }
        });
        ui.label(match &snapshot.uploader {
            UploaderState::Idle => "전송 준비 · 대기 중".into(),
            UploaderState::Draining => "전송 중".into(),
            UploaderState::Gated => "설정을 완료하면 전송을 시작합니다.".into(),
            UploaderState::Suspended => {
                "반복 실패로 전송 대기 · 수동 전송 / 새 조각 / 연결 복구 시 재개".into()
            }
            UploaderState::Backoff { until, attempt } => format!(
                "재시도 {attempt}회 · 약 {}초 후 재개 (상태 갱신 시점 기준)",
                until
                    .saturating_duration_since(tokio::time::Instant::now())
                    .as_secs()
            ),
        });
        if let Some(error) = &snapshot.last_error {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!(
                    "{} · {}: {}",
                    error.at.format("%H:%M:%S"),
                    error.component,
                    error.message
                ),
            );
        }
        for error in &snapshot.watch_errors {
            ui.colored_label(
                egui::Color32::LIGHT_RED,
                format!("{}: {}", error.pattern, error.reason),
            );
        }
        ui.add_space(12.0);
        ui.heading(format!("감시 파일 {}개", snapshot.files.len()));
        use egui_extras::{Column, TableBuilder};
        let mut reset = None;
        TableBuilder::new(ui)
            .striped(true)
            .resizable(true)
            .column(Column::remainder().at_least(200.0))
            .column(Column::auto())
            .column(Column::auto())
            .column(Column::initial(110.0))
            .column(Column::auto())
            .column(Column::auto())
            .column(Column::auto())
            .max_scroll_height(280.0)
            .header(24.0, |mut header| {
                for label in [
                    "경로",
                    "크기",
                    "offset",
                    "진행률",
                    "마지막 처리",
                    "헤더",
                    "초기화",
                ] {
                    header.col(|ui| {
                        ui.strong(label);
                    });
                }
            })
            .body(|body| {
                body.rows(24.0, snapshot.files.len(), |mut row| {
                    let file = &snapshot.files[row.index()];
                    row.col(|ui| {
                        if ui
                            .selectable_label(
                                self.selected_file.as_deref() == Some(file.file_key.as_str()),
                                file.path.display().to_string(),
                            )
                            .on_hover_text(file.last_error.as_deref().unwrap_or(if file.present {
                                ""
                            } else {
                                "현재 파일을 찾지 못했습니다."
                            }))
                            .clicked()
                        {
                            self.selected_file = Some(file.file_key.clone());
                        }
                    });
                    row.col(|ui| {
                        ui.label(bytes(file.size));
                    });
                    row.col(|ui| {
                        ui.label(file.offset.to_string());
                    });
                    row.col(|ui| {
                        ui.add(
                            egui::ProgressBar::new(if file.size == 0 {
                                0.0
                            } else {
                                (file.offset as f64 / file.size as f64).min(1.0) as f32
                            })
                            .show_percentage(),
                        );
                    });
                    row.col(|ui| {
                        ui.label(
                            file.last_processed
                                .map(|time| time.format("%H:%M:%S").to_string())
                                .unwrap_or_else(|| "—".into()),
                        );
                    });
                    row.col(|ui| {
                        ui.label(file.header_kind.map(|kind| kind.as_str()).unwrap_or("—"));
                    });
                    row.col(|ui| {
                        if ui
                            .add_enabled(file.present, egui::Button::new("offset 리셋").small())
                            .on_hover_text("이 파일을 처음부터 다시 업로드합니다.")
                            .on_disabled_hover_text("현재 파일을 찾지 못해 다시 보낼 수 없습니다.")
                            .clicked()
                        {
                            reset = Some(Confirm::Reset {
                                key: file.file_key.clone(),
                                path: file.path.display().to_string(),
                            });
                        }
                    });
                })
            });
        // A missing source has nothing to re-send, so a full reset covers present files only.
        let present: Vec<String> = snapshot
            .files
            .iter()
            .filter(|file| file.present)
            .map(|file| file.file_key.clone())
            .collect();
        if ui
            .add_enabled(!present.is_empty(), egui::Button::new("전체 offset 리셋…"))
            .on_hover_text("감시 중인 모든 파일을 처음부터 다시 업로드합니다.")
            .clicked()
        {
            reset = Some(Confirm::ResetAll { keys: present });
        }
        if let Some(confirm) = reset {
            self.confirm = Some(confirm);
            self.focus_cancel = true;
        }
    }
}
