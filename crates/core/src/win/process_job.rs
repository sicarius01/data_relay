//! Child processes are created suspended, assigned to a Job Object, then resumed.
//! Assignment before execution prevents a fast child from escaping the job.
#[cfg(not(windows))]
use anyhow::Result;

#[cfg(windows)]
mod platform {
    use anyhow::{bail, Context, Result};
    use windows::{
        core::PCWSTR,
        Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::{
                Diagnostics::ToolHelp::{
                    CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD,
                    THREADENTRY32,
                },
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                    SetInformationJobObject, TerminateJobObject,
                    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                },
                Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME},
            },
        },
    };

    struct OwnedHandle(HANDLE);
    impl Drop for OwnedHandle {
        fn drop(&mut self) {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
    // Kernel handles can be used and closed on any thread. Each OwnedHandle is
    // uniquely owned, so its destructor cannot race with access through &self.
    unsafe impl Send for OwnedHandle {}
    unsafe impl Sync for OwnedHandle {}

    pub struct ProcessJob(OwnedHandle);
    impl ProcessJob {
        /// `child` MUST have been spawned with CREATE_SUSPENDED.
        pub fn attach(child: &tokio::process::Child) -> Result<Self> {
            let pid = child.id().context("child already exited")?;
            let process = HANDLE(child.raw_handle().context("child has no process handle")?);
            unsafe {
                let job = Self(OwnedHandle(CreateJobObjectW(None, PCWSTR::null())?));
                let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
                limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
                SetInformationJobObject(
                    job.0 .0,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )?;
                AssignProcessToJobObject(job.0 .0, process)
                    .context("assign child to Job Object")?;
                resume_initial_thread(pid)?;
                Ok(job)
            }
        }
        pub fn terminate(&self) -> Result<()> {
            unsafe {
                TerminateJobObject(self.0 .0, 1)?;
            }
            Ok(())
        }
    }

    unsafe fn resume_initial_thread(pid: u32) -> Result<()> {
        let snapshot = OwnedHandle(CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)?);
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        Thread32First(snapshot.0, &mut entry)?;
        loop {
            if entry.th32OwnerProcessID == pid {
                let thread = OwnedHandle(OpenThread(
                    THREAD_SUSPEND_RESUME,
                    false,
                    entry.th32ThreadID,
                )?);
                if ResumeThread(thread.0) == u32::MAX {
                    return Err(std::io::Error::last_os_error().into());
                }
                return Ok(());
            }
            if Thread32Next(snapshot.0, &mut entry).is_err() {
                break;
            }
        }
        bail!("cannot find suspended primary thread for child {pid}")
    }
}

#[cfg(windows)]
pub use platform::ProcessJob;

#[cfg(not(windows))]
pub struct ProcessJob;
#[cfg(not(windows))]
impl ProcessJob {
    pub fn attach(_: &tokio::process::Child) -> Result<Self> {
        Ok(Self)
    }
    pub fn terminate(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::ProcessJob;
    use std::{process::Stdio, time::Duration};
    use tokio::io::{AsyncBufReadExt, BufReader};
    use windows::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0},
        System::Threading::{
            OpenProcess, WaitForSingleObject, CREATE_NO_WINDOW, CREATE_SUSPENDED,
            PROCESS_SYNCHRONIZE,
        },
    };

    #[tokio::test]
    async fn suspended_child_is_resumed_and_entire_tree_is_terminated() {
        let mut command = tokio::process::Command::new("powershell.exe");
        command.args(["-NoProfile", "-NonInteractive", "-Command",
            "$jobChild = Start-Process powershell.exe -ArgumentList '-NoProfile -NonInteractive -Command Start-Sleep -Seconds 60' -WindowStyle Hidden -PassThru; [Console]::WriteLine($jobChild.Id); Start-Sleep -Seconds 60"])
            .creation_flags(CREATE_NO_WINDOW.0 | CREATE_SUSPENDED.0)
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
        let mut child = command.spawn().unwrap();
        let job = ProcessJob::attach(&child).unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        tokio::time::timeout(Duration::from_secs(15), output.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        let descendant_pid: u32 = line.trim().parse().expect("child process id on stdout");
        let descendant =
            unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, descendant_pid).unwrap() };
        job.terminate().unwrap();
        tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .unwrap()
            .unwrap();
        let result = unsafe { WaitForSingleObject(descendant, 5000) };
        unsafe {
            CloseHandle(descendant).unwrap();
        }
        assert_eq!(result, WAIT_OBJECT_0, "descendant must be terminated too");
    }
}
