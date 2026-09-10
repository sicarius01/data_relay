use crate::events::{EventSink, UiEvent};
use csv_agent_core::bus::StatusSnapshot;

#[cfg(windows)]
pub struct Tray {
    icon: tray_icon::TrayIcon,
    state: String,
    tooltip: String,
}

#[cfg(windows)]
impl Tray {
    pub fn new(sink: EventSink) -> anyhow::Result<Self> {
        use tray_icon::{
            menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem},
            TrayIconBuilder, TrayIconEvent,
        };
        let menu = Menu::new();
        let show = MenuItem::new("설정 열기", true, None);
        let flush = MenuItem::new("지금 전송", true, None);
        let logs = MenuItem::new("로그 폴더 열기", true, None);
        let exit = MenuItem::new("종료", true, None);
        menu.append_items(&[
            &show,
            &flush,
            &logs,
            &PredefinedMenuItem::separator(),
            &exit,
        ])?;
        let (show_id, flush_id, logs_id, exit_id) = (
            show.id().clone(),
            flush.id().clone(),
            logs.id().clone(),
            exit.id().clone(),
        );
        let menu_sink = sink.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let event = if event.id == show_id {
                UiEvent::Show
            } else if event.id == flush_id {
                UiEvent::Flush
            } else if event.id == logs_id {
                UiEvent::Logs
            } else if event.id == exit_id {
                UiEvent::Exit
            } else {
                return;
            };
            menu_sink.send(event);
        }));
        TrayIconEvent::set_event_handler(Some(move |event| {
            if matches!(event, TrayIconEvent::DoubleClick { .. }) {
                sink.send(UiEvent::Show);
            }
        }));
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("CSV 수집 에이전트")
            .with_icon(make_icon([35, 169, 114])?)
            .build()?;
        Ok(Self {
            icon,
            state: String::new(),
            tooltip: String::new(),
        })
    }

    pub fn sync(&mut self, snapshot: &StatusSnapshot) {
        let state = format!("{:?}", snapshot.tray);
        if self.state != state {
            tracing::debug!(%state, "tray state changed");
            let color = match state.as_str() {
                "Error" => [221, 72, 76],
                "Pending" => [233, 166, 44],
                _ => [35, 169, 114],
            };
            if let Ok(icon) = make_icon(color) {
                let _ = self.icon.set_icon(Some(icon));
            }
            self.state = state;
        }
        let last = snapshot
            .last_upload_at
            .map(|t| t.format("%H:%M").to_string())
            .unwrap_or_else(|| "없음".into());
        let tooltip = format!(
            "{} · 마지막 전송 {} · 대기 {}건",
            snapshot.equip_id.as_deref().unwrap_or("설정 필요"),
            last,
            snapshot.spool_pending
        );
        if tooltip != self.tooltip {
            tracing::debug!(%tooltip, "tray tooltip changed");
            let _ = self.icon.set_tooltip(Some(&tooltip));
            self.tooltip = tooltip;
        }
    }
}

#[cfg(windows)]
fn make_icon(color: [u8; 3]) -> anyhow::Result<tray_icon::Icon> {
    // Small geometric data-sheet icon, generated directly as RGBA without assets.
    let mut pixels = vec![0; 32 * 32 * 4];
    for y in 3usize..29 {
        for x in 5usize..27 {
            let i = (y * 32 + x) * 4;
            let line = (y == 10 || y == 16 || y == 22) && (10..23).contains(&x);
            let pixel = if line {
                [255, 255, 255, 255]
            } else {
                [color[0], color[1], color[2], 255]
            };
            pixels[i..i + 4].copy_from_slice(&pixel);
        }
    }
    Ok(tray_icon::Icon::from_rgba(pixels, 32, 32)?)
}

#[cfg(not(windows))]
pub struct Tray;
#[cfg(not(windows))]
impl Tray {
    pub fn new(_: EventSink) -> anyhow::Result<Self> {
        Ok(Self)
    }
    pub fn sync(&mut self, _: &StatusSnapshot) {}
}
