use crate::events::{EventSink, UiEvent};

pub fn open_logs(path: &std::path::Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(windows)]
    std::process::Command::new("explorer.exe")
        .arg(path)
        .spawn()?;
    #[cfg(not(windows))]
    std::process::Command::new("xdg-open").arg(path).spawn()?;
    Ok(())
}

#[cfg(windows)]
mod native {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    };
    use windows::{
        core::w,
        Win32::{
            Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE, WAIT_OBJECT_0},
            System::{
                Power::{
                    PowerRegisterSuspendResumeNotification,
                    PowerUnregisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
                    HPOWERNOTIFY,
                },
                Threading::{CreateEventW, CreateMutexW, SetEvent, WaitForSingleObject, INFINITE},
            },
            UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC},
        },
    };

    pub struct SingleInstance {
        mutex: HANDLE,
        event: HANDLE,
        stop: Arc<AtomicBool>,
        waiter: Option<std::thread::JoinHandle<()>>,
    }

    impl SingleInstance {
        pub fn acquire(sink: EventSink) -> anyhow::Result<Option<Self>> {
            unsafe {
                // Create the show event first: another process can signal it even while
                // this process is still constructing the UI. One agent per session also
                // prevents two executable copies from concurrently opening the same DB.
                let event = CreateEventW(None, false, false, w!("Local\\CsvAgent.Show"))?;
                let mutex = match CreateMutexW(None, false, w!("Local\\CsvAgent.Instance")) {
                    Ok(handle) => handle,
                    Err(error) => {
                        let _ = CloseHandle(event);
                        return Err(error.into());
                    }
                };
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let result = SetEvent(event);
                    let _ = CloseHandle(event);
                    let _ = CloseHandle(mutex);
                    result?;
                    return Ok(None);
                }
                let stop = Arc::new(AtomicBool::new(false));
                let stopped = stop.clone();
                let raw = event.0 as usize;
                let waiter = std::thread::Builder::new()
                    .name("single-instance".into())
                    .spawn(move || {
                        let event = HANDLE(raw as *mut _);
                        while WaitForSingleObject(event, INFINITE) == WAIT_OBJECT_0 {
                            if stopped.load(Ordering::Acquire) {
                                break;
                            }
                            sink.send(UiEvent::Show);
                        }
                    })?;
                Ok(Some(Self {
                    mutex,
                    event,
                    stop,
                    waiter: Some(waiter),
                }))
            }
        }
    }

    impl Drop for SingleInstance {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Release);
            unsafe {
                let _ = SetEvent(self.event);
            }
            if let Some(waiter) = self.waiter.take() {
                let _ = waiter.join();
            }
            unsafe {
                let _ = CloseHandle(self.event);
                let _ = CloseHandle(self.mutex);
            }
        }
    }

    struct ResumeContext {
        sender: tokio::sync::mpsc::UnboundedSender<csv_agent_core::bus::Command>,
    }

    pub struct PowerNotifications {
        registration: HPOWERNOTIFY,
        // Kept alive until notification callbacks have been unregistered.
        _context: Box<ResumeContext>,
    }

    unsafe extern "system" fn on_power(
        context: *const std::ffi::c_void,
        kind: u32,
        _: *const std::ffi::c_void,
    ) -> u32 {
        if kind == PBT_APMRESUMEAUTOMATIC && !context.is_null() {
            let context = unsafe { &*(context as *const ResumeContext) };
            let _ = context
                .sender
                .send(csv_agent_core::bus::Command::SystemResumed);
        }
        0
    }

    impl PowerNotifications {
        pub fn register(
            sender: tokio::sync::mpsc::UnboundedSender<csv_agent_core::bus::Command>,
        ) -> anyhow::Result<Self> {
            let mut context = Box::new(ResumeContext { sender });
            let parameters = DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power),
                Context: (&mut *context as *mut ResumeContext).cast(),
            };
            let mut registration = std::ptr::null_mut();
            unsafe {
                PowerRegisterSuspendResumeNotification(
                    DEVICE_NOTIFY_CALLBACK,
                    HANDLE(
                        (&parameters as *const DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS)
                            .cast_mut()
                            .cast(),
                    ),
                    &mut registration,
                )
                .ok()?;
            }
            Ok(Self {
                registration: HPOWERNOTIFY(registration as isize),
                _context: context,
            })
        }
    }

    impl Drop for PowerNotifications {
        fn drop(&mut self) {
            unsafe {
                let _ = PowerUnregisterSuspendResumeNotification(self.registration);
            }
        }
    }
}

#[cfg(windows)]
pub use native::*;

#[cfg(not(windows))]
pub struct SingleInstance;
#[cfg(not(windows))]
impl SingleInstance {
    pub fn acquire(_: EventSink) -> anyhow::Result<Option<Self>> {
        Ok(Some(Self))
    }
}
