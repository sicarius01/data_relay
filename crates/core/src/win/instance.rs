use std::path::Path;

/// Cross-process lifetime lock for a state database. Holding the named kernel
/// object (without owning its wait state) permits moving the guard to the runtime
/// thread; mutex ownership itself must never be released from a different thread.
pub struct InstanceLock {
    #[cfg(windows)]
    handle: usize,
}

impl InstanceLock {
    pub fn acquire(state_path: &Path) -> anyhow::Result<Self> {
        #[cfg(windows)]
        {
            use windows::{
                core::PCWSTR,
                Win32::{
                    Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS},
                    System::Threading::CreateMutexW,
                },
            };
            let absolute = if state_path.is_absolute() {
                state_path.to_path_buf()
            } else {
                std::env::current_dir()?.join(state_path)
            };
            let parent = absolute.parent().unwrap_or(Path::new("."));
            std::fs::create_dir_all(parent)?;
            let canonical_parent = std::fs::canonicalize(parent)?;
            let canonical = canonical_parent.join(absolute.file_name().unwrap_or_default());
            let normalized = canonical
                .to_string_lossy()
                .replace('/', "\\")
                .to_lowercase();
            // FNV-1a is stable across processes and Rust versions.
            let hash = normalized
                .as_bytes()
                .iter()
                .fold(0xcbf29ce484222325u64, |hash, byte| {
                    (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
                });
            let name: Vec<u16> = format!("Global\\CsvAgent.State.{hash:016x}")
                .encode_utf16()
                .chain(Some(0))
                .collect();
            unsafe {
                let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))?;
                if GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    anyhow::bail!(
                        "이 상태 데이터베이스를 사용하는 에이전트가 이미 실행 중입니다: {}",
                        state_path.display()
                    );
                }
                Ok(Self {
                    handle: handle.0 as usize,
                })
            }
        }
        #[cfg(not(windows))]
        {
            let _ = state_path;
            Ok(Self {})
        }
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        #[cfg(windows)]
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(windows::Win32::Foundation::HANDLE(
                self.handle as *mut _,
            ));
        }
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn named_lock_rejects_duplicate_path_and_releases_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let state = directory.path().join("state.db");
        let lock = InstanceLock::acquire(&state).unwrap();
        assert!(InstanceLock::acquire(&directory.path().join(".").join("STATE.DB")).is_err());
        let another = InstanceLock::acquire(&directory.path().join("other.db")).unwrap();
        drop(another);
        drop(lock);
        assert!(InstanceLock::acquire(&state).is_ok());
    }
}
