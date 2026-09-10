use std::{fs::File, io, path::Path};
pub mod console_viewer;
pub mod process_job;
pub use process_job::ProcessJob;
pub mod instance;
pub use instance::InstanceLock;
pub type FileKey = String;
pub const CREATE_NO_WINDOW: u32 = 0x08000000;

fn io_error_hint(error: &io::Error) -> Option<&'static str> {
    match error.raw_os_error()? {
        5 => Some("접근 권한이 없습니다. 현재 로그인 계정의 파일·공유 폴더 권한을 확인하세요."),
        2 | 3 => Some("파일 또는 경로를 찾을 수 없습니다. 설정 경로와 드라이브 연결을 확인하세요."),
        53 | 67 => Some("네트워크 경로를 찾을 수 없습니다. 서버 주소·공유 이름·네트워크 연결을 확인하세요."),
        1219 => Some("서로 다른 자격 증명으로 같은 서버에 연결되어 있습니다. 기존 공유 연결과 로그인 계정을 확인하세요."),
        1326 => Some("서버 로그인에 실패했습니다. 공유 폴더에 사용하는 사용자 이름과 비밀번호를 확인하세요."),
        32 => Some("다른 프로그램이 파일을 독점 사용 중입니다. CSV 생성 프로그램의 공유 읽기 허용 여부를 확인하세요."),
        112 => Some("디스크 공간이 부족합니다. 로컬 스풀 또는 서버 공유 드라이브의 여유 공간을 확인하세요."),
        _ => None,
    }
}

/// A actionable local-language explanation while retaining the OS diagnostic.
pub fn describe(error: &io::Error) -> String {
    match io_error_hint(error) {
        Some(hint) => format!("{hint} ({error})"),
        None => error.to_string(),
    }
}

/// Preserve the complete operation/path context and explain a nested IO failure.
pub fn describe_error(error: &anyhow::Error) -> String {
    let details = format!("{error:#}");
    match error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .find_map(io_error_hint)
    {
        Some(hint) => format!("{details}\n{hint}"),
        None => details,
    }
}

#[cfg(windows)]
pub fn file_key(file: &File) -> io::Result<FileKey> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::{
        Foundation::HANDLE,
        Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION},
    };
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe { GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut info) }
        .map_err(io::Error::other)?;
    Ok(format!(
        "{:08x}:{:016x}",
        info.dwVolumeSerialNumber,
        ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64
    ))
}
#[cfg(unix)]
pub fn file_key(file: &File) -> io::Result<FileKey> {
    use std::os::unix::fs::MetadataExt;
    let m = file.metadata()?;
    Ok(format!("{:08x}:{:016x}", m.dev(), m.ino()))
}
#[cfg(not(any(windows, unix)))]
pub fn file_key(_: &File) -> io::Result<FileKey> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "file identity unsupported",
    ))
}
pub fn replace_file(from: &Path, to: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows::{
            core::PCWSTR,
            Win32::Storage::FileSystem::{
                MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
            },
        };
        let a: Vec<_> = from.as_os_str().encode_wide().chain(Some(0)).collect();
        let b: Vec<_> = to.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            MoveFileExW(
                PCWSTR(a.as_ptr()),
                PCWSTR(b.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        }
        .map_err(io::Error::other)
    }
    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)?;
        if let Some(parent) = to.parent() {
            File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}
pub fn set_autostart(enabled: bool) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new("reg.exe");
        if enabled {
            let data_dir = autostart_data_dir(std::env::args_os().skip(1))?
                .map(std::path::absolute)
                .transpose()?;
            let command_line = autostart_command(&exe, data_dir.as_deref())?;
            cmd.args([
                "add",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "csv-agent",
                "/t",
                "REG_SZ",
                "/d",
            ])
            .arg(command_line)
            .arg("/f");
        } else {
            let present = std::process::Command::new("reg.exe")
                .args([
                    "query",
                    r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                    "/v",
                    "csv-agent",
                ])
                .creation_flags(CREATE_NO_WINDOW)
                .output()?;
            if !present.status.success() {
                return Ok(());
            }
            cmd.args([
                "delete",
                r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                "/v",
                "csv-agent",
                "/f",
            ]);
        }
        let out = cmd.creation_flags(CREATE_NO_WINDOW).output()?;
        if !out.status.success() {
            anyhow::bail!(
                "autostart registry change failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        Ok(())
    }
    #[cfg(not(windows))]
    {
        let _ = enabled;
        Ok(())
    }
}

#[cfg(windows)]
fn autostart_data_dir(
    args: impl IntoIterator<Item = std::ffi::OsString>,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let mut args = args.into_iter();
    let mut data_dir = None;
    while let Some(arg) = args.next() {
        if arg == "--data-dir" {
            let value = args
                .next()
                .ok_or_else(|| anyhow::anyhow!("--data-dir requires a directory"))?;
            anyhow::ensure!(!value.is_empty(), "--data-dir requires a directory");
            data_dir = Some(std::path::PathBuf::from(value));
        }
    }
    Ok(data_dir)
}

/// Construct a Run value without shell expansion or lossy UTF-16 conversion.
#[cfg(windows)]
fn autostart_command(
    executable: &Path,
    data_dir: Option<&Path>,
) -> anyhow::Result<std::ffi::OsString> {
    use std::os::windows::ffi::OsStringExt;
    let mut command = quote_windows_argument(executable.as_os_str())?;
    command.extend(" --hidden".encode_utf16());
    if let Some(data_dir) = data_dir {
        anyhow::ensure!(
            data_dir.is_absolute(),
            "autostart data directory must be absolute"
        );
        command.extend(" --data-dir ".encode_utf16());
        command.extend(quote_windows_argument(data_dir.as_os_str())?);
    }
    Ok(std::ffi::OsString::from_wide(&command))
}

#[cfg(windows)]
fn quote_windows_argument(argument: &std::ffi::OsStr) -> anyhow::Result<Vec<u16>> {
    use std::os::windows::ffi::OsStrExt;
    // Microsoft CRT parsing rules: double backslashes before the closing quote;
    // before a literal quote use 2n+1 backslashes. Other backslashes are literal.
    // https://learn.microsoft.com/en-us/cpp/c-language/parsing-c-command-line-arguments
    let mut quoted = vec![b'"' as u16];
    let mut backslashes = 0;
    for unit in argument.encode_wide() {
        anyhow::ensure!(unit != 0, "command-line argument contains a NUL");
        if unit == b'\\' as u16 {
            backslashes += 1;
            continue;
        }
        let count = if unit == b'"' as u16 {
            backslashes * 2 + 1
        } else {
            backslashes
        };
        quoted.extend(std::iter::repeat_n(b'\\' as u16, count));
        quoted.push(unit);
        backslashes = 0;
    }
    quoted.extend(std::iter::repeat_n(b'\\' as u16, backslashes * 2));
    quoted.push(b'"' as u16);
    Ok(quoted)
}

#[cfg(all(test, windows))]
mod autostart_tests {
    use super::*;
    use std::ffi::{OsStr, OsString};

    #[test]
    fn default_login_is_hidden_and_custom_directory_is_preserved() {
        let exe = Path::new(r"C:\Program Files\csv-agent.exe");
        assert_eq!(
            autostart_command(exe, None).unwrap(),
            OsStr::new("\"C:\\Program Files\\csv-agent.exe\" --hidden")
        );
        assert_eq!(
            autostart_command(exe, Some(Path::new("D:\\장비 자료\\"))).unwrap(),
            OsStr::new(
                "\"C:\\Program Files\\csv-agent.exe\" --hidden --data-dir \"D:\\장비 자료\\\\\""
            )
        );
        assert!(autostart_command(exe, Some(Path::new("relative"))).is_err());
    }

    #[test]
    fn selection_matches_gui_last_directory_and_drops_show_flag() {
        let args = ["--data-dir", "first", "--show", "--data-dir", "second"].map(OsString::from);
        assert_eq!(
            autostart_data_dir(args).unwrap(),
            Some(std::path::PathBuf::from("second"))
        );
        assert!(autostart_data_dir([OsString::from("--data-dir")]).is_err());
        assert!(autostart_data_dir([OsString::from("--show")])
            .unwrap()
            .is_none());
    }

    #[test]
    fn argument_quotes_escape_literal_quotes_and_trailing_backslashes() {
        let encoded = quote_windows_argument(OsStr::new("a\\\"b\\")).unwrap();
        assert_eq!(String::from_utf16(&encoded).unwrap(), "\"a\\\\\\\"b\\\\\"");
        assert_eq!(
            String::from_utf16(&quote_windows_argument(OsStr::new("")).unwrap()).unwrap(),
            "\"\""
        );
        assert!(quote_windows_argument(OsStr::new("a\0b")).is_err());
    }
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn known_windows_errors_have_recovery_guidance_and_preserve_context() {
        for (code, text) in [
            (5, "권한"),
            (2, "경로"),
            (3, "경로"),
            (53, "네트워크"),
            (67, "네트워크"),
            (1219, "자격 증명"),
            (1326, "로그인"),
            (32, "공유 읽기"),
            (112, "디스크 공간"),
        ] {
            assert!(describe(&io::Error::from_raw_os_error(code)).contains(text));
        }
        let error =
            anyhow::Error::new(io::Error::from_raw_os_error(32)).context("open D:/logs/source.csv");
        let message = describe_error(&error);
        assert!(message.contains("open D:/logs/source.csv"));
        assert!(message.contains("공유 읽기"));
        let unknown = io::Error::from_raw_os_error(9999);
        assert_eq!(describe(&unknown), unknown.to_string());
        assert_eq!(
            describe_error(&anyhow::anyhow!("ordinary error")),
            "ordinary error"
        );
    }
}
