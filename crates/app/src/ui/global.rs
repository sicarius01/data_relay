use super::*;

impl AgentApp {
    pub(super) fn global(&mut self, ui: &mut egui::Ui) {
        ui.heading("장비 및 업로드 경로");
        ui.label("장비 번호와 이름의 조합은 다른 PC와 중복되지 않아야 합니다.");
        egui::Grid::new("equipment_grid")
            .num_columns(2)
            .spacing([20.0, 10.0])
            .show(ui, |ui| {
                text_row(ui, "장비 번호 *", &mut self.draft.equip.num);
                text_row(ui, "장비 이름 *", &mut self.draft.equip.name);
                text_row(ui, "ID 결합 구분자", &mut self.draft.equip.id_separator);
                text_row(ui, "UNC 기본 경로 *", &mut self.draft.server.unc_base);
            });
        self.connection_probe
            .invalidate_changed_base(&self.draft.server.unc_base);
        match self.draft.equip_id() {
            Ok(id) => {
                ui.label(format!("장비 ID: {id}"));
                ui.monospace(format!(
                    "{}\\raw\\{}\\{}\\",
                    self.draft.server.unc_base.trim_end_matches('\\'),
                    id,
                    chrono::Local::now().format("%Y%m%d")
                ));
            }
            Err(errors) => {
                for error in errors {
                    ui.colored_label(egui::Color32::LIGHT_RED, error.to_string());
                }
            }
        }
        ui.horizontal(|ui| {
            if ui
                .add_enabled(
                    !self.connection_probe.pending(),
                    egui::Button::new("연결 테스트"),
                )
                .clicked()
            {
                let reply = self
                    .connection_probe
                    .begin(self.draft.server.unc_base.clone());
                self.command(Command::TestUnc {
                    base: self.draft.server.unc_base.clone(),
                    reply,
                });
            }
            if self.connection_probe.pending() {
                ui.label("임시 파일 쓰기 / 이름 변경 / 삭제 확인 중…");
            }
            if let Some(result) = &self.connection_probe.result {
                match result {
                    Ok(()) => {
                        ui.colored_label(
                            egui::Color32::from_rgb(25, 130, 80),
                            "연결 성공: 입력 경로의 쓰기 · 이름 변경 · 삭제 확인",
                        );
                    }
                    Err(error) => {
                        ui.colored_label(egui::Color32::LIGHT_RED, error);
                    }
                }
            }
        });
        if self.draft.equip != self.applied.equip || self.draft.server != self.applied.server {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(
                    ui.visuals().warn_fg_color,
                    "설정 저장 필요 · 연결 테스트는 설정을 저장하지 않습니다.",
                );
                if ui.button("저장 및 적용").clicked() {
                    self.save();
                }
            });
        }
        ui.add_space(16.0);
        ui.separator();
        ui.heading("저장 및 운영 설정");
        egui::Grid::new("global_grid")
            .num_columns(2)
            .spacing([20.0, 10.0])
            .show(ui, |ui| {
                text_row(
                    ui,
                    "스풀 디렉토리 · 재시작 후 적용",
                    &mut self.draft.agent.spool_dir,
                );
                number_row(
                    ui,
                    "스풀 용량 상한 (bytes)",
                    &mut self.draft.agent.spool_max_bytes,
                );
                number_row(
                    ui,
                    "조각 최대 크기 (bytes)",
                    &mut self.draft.agent.max_chunk_bytes,
                );
                number_row(
                    ui,
                    "하트비트 주기 (초)",
                    &mut self.draft.agent.heartbeat_interval_sec,
                );
                number_row(ui, "전송 재시도 상한", &mut self.draft.server.retry_max);
                number_row(
                    ui,
                    "재시도 기본 대기 (ms)",
                    &mut self.draft.server.retry_base_ms,
                );
                ui.label("자동 시작");
                ui.checkbox(&mut self.draft.agent.autostart, "Windows 로그인 시 실행");
                ui.end_row();
                text_row(
                    ui,
                    "로그 디렉토리 · 재시작 후 적용",
                    &mut self.draft.log.dir,
                );
                ui.label("로그 레벨");
                egui::ComboBox::from_id_salt("log_level")
                    .selected_text(&self.draft.log.level)
                    .show_ui(ui, |ui| {
                        for level in ["trace", "debug", "info", "warn", "error"] {
                            ui.selectable_value(&mut self.draft.log.level, level.into(), level);
                        }
                    });
                ui.end_row();
                number_row(
                    ui,
                    "로그 보관 파일 수 · 재시작 후 적용",
                    &mut self.draft.log.max_files,
                );
                ui.label("종료 확인 추가 문구");
                ui.add(
                    egui::TextEdit::multiline(&mut self.draft.ui.exit_confirm_message)
                        .desired_rows(3)
                        .desired_width(540.0),
                );
                ui.end_row();
            });
        if ui.button("로그 폴더 열기").clicked() {
            self.logs();
        }
        for error in self.draft.validate().into_iter().filter(|error| {
            !error.field.starts_with("equip")
                && !error.field.starts_with("task")
                && !error.field.starts_with("watch")
        }) {
            ui.colored_label(egui::Color32::LIGHT_RED, error.to_string());
        }
    }
}
