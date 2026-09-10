#[cfg(windows)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug)]
pub enum UiEvent {
    Show,
    Flush,
    Logs,
    Exit,
    ShutdownFinished(Result<(), String>),
}

/// Producers wake the native event loop only when there is actual work.
#[derive(Clone, Default)]
pub struct EventSink {
    queue: Arc<Mutex<Vec<UiEvent>>>,
    context: Arc<Mutex<Option<egui::Context>>>,
    #[cfg(windows)]
    window: Arc<AtomicUsize>,
}

impl EventSink {
    pub fn attach(&self, context: egui::Context) {
        *self.context.lock().unwrap() = Some(context);
    }

    #[cfg(windows)]
    pub fn attach_window(&self, handle: usize) {
        self.window.store(handle, Ordering::Release);
    }

    pub fn detach(&self) {
        #[cfg(windows)]
        self.window.store(0, Ordering::Release);
        *self.context.lock().unwrap() = None;
    }

    pub fn wake(&self) {
        if let Some(context) = self.context.lock().unwrap().as_ref() {
            context.request_repaint();
        }
        #[cfg(windows)]
        {
            use windows::Win32::{
                Foundation::{HWND, LPARAM, WPARAM},
                UI::WindowsAndMessaging::{IsWindowVisible, PostMessageW, WM_PAINT},
            };
            let handle = self.window.load(Ordering::Acquire);
            if handle != 0 {
                let window = HWND(handle as *mut _);
                unsafe {
                    // Winit's normal request_redraw uses RedrawWindow, which Windows
                    // suppresses for hidden windows. A real producer event explicitly
                    // wakes the hidden viewport once; update then synchronizes the tray
                    // and returns without constructing the UI. There is no idle timer.
                    if !IsWindowVisible(window).as_bool() {
                        let _ = PostMessageW(Some(window), WM_PAINT, WPARAM(0), LPARAM(0));
                    }
                }
            }
        }
    }

    pub fn send(&self, event: UiEvent) {
        self.queue.lock().unwrap().push(event);
        self.wake();
    }

    pub fn drain(&self) -> Vec<UiEvent> {
        std::mem::take(&mut *self.queue.lock().unwrap())
    }
}
