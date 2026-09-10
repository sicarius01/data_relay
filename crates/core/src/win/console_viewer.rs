//! Separate console process for test output; closing it cannot close the tray app.

#[cfg(windows)]
pub fn run() -> anyhow::Result<()> {
    use anyhow::{ensure, Context};
    use std::{
        fs::{File, OpenOptions},
        io::{Read, Write},
        os::windows::io::{AsRawHandle, FromRawHandle},
    };
    use windows::{
        core::w,
        Win32::{
            Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE},
            System::{
                Console::{
                    AllocConsole, FreeConsole, GetStdHandle, ReadConsoleW, SetConsoleTitleW,
                    WriteConsoleW, STD_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
                },
                Threading::GetCurrentProcess,
            },
        },
    };

    fn duplicate_standard_handle(which: STD_HANDLE) -> anyhow::Result<File> {
        unsafe {
            let source = GetStdHandle(which)?;
            ensure!(!source.is_invalid(), "missing viewer pipe");
            let process = GetCurrentProcess();
            let mut duplicate = HANDLE::default();
            DuplicateHandle(
                process,
                source,
                process,
                &mut duplicate,
                0,
                false,
                DUPLICATE_SAME_ACCESS,
            )?;
            Ok(File::from_raw_handle(duplicate.0))
        }
    }

    fn display(output: &File, text: &str) -> anyhow::Result<()> {
        let wide: Vec<u16> = text.encode_utf16().collect();
        for chunk in wide.chunks(4096) {
            let mut remaining = chunk;
            while !remaining.is_empty() {
                let mut written = 0;
                unsafe {
                    WriteConsoleW(
                        HANDLE(output.as_raw_handle()),
                        remaining,
                        Some(&mut written),
                        None,
                    )?;
                }
                ensure!(written > 0, "console stopped accepting output");
                remaining = &remaining[written as usize..];
            }
        }
        Ok(())
    }

    // AllocConsole replaces standard handles. Preserve both communication pipes first.
    let mut input = duplicate_standard_handle(STD_INPUT_HANDLE)?;
    let mut ready = duplicate_standard_handle(STD_OUTPUT_HANDLE)?;
    unsafe {
        // CREATE_NO_WINDOW console executables may already own a console with
        // no HWND. Detach that console as well before allocating a visible one.
        let _ = FreeConsole();
        AllocConsole().context("cannot open test console")?;
        SetConsoleTitleW(w!("CSV 수집 에이전트 - 테스트 실행"))?;
    }
    let output = OpenOptions::new().write(true).open("CONOUT$")?;
    ready.write_all(b"READY\n")?;
    ready.flush()?;
    drop(ready);

    // Stream even output without a newline, retaining split UTF-8 sequences.
    let mut pending = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            display(&output, &String::from_utf8_lossy(&pending))?;
            break;
        }
        pending.extend_from_slice(&buffer[..count]);
        loop {
            match std::str::from_utf8(&pending) {
                Ok(text) => {
                    display(&output, text)?;
                    pending.clear();
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    display(&output, std::str::from_utf8(&pending[..valid])?)?;
                    pending.drain(..valid);
                    if let Some(invalid) = error.error_len() {
                        display(&output, "\u{fffd}")?;
                        pending.drain(..invalid);
                    } else {
                        break;
                    }
                }
            }
        }
    }
    display(
        &output,
        "\r\n테스트가 끝났습니다. Enter 키를 누르면 창을 닫습니다.\r\n",
    )?;
    let keyboard = OpenOptions::new().read(true).write(true).open("CONIN$")?;
    let mut key_buffer = [0u16; 128];
    let mut read = 0;
    unsafe {
        ReadConsoleW(
            HANDLE(keyboard.as_raw_handle()),
            key_buffer.as_mut_ptr().cast(),
            key_buffer.len() as u32,
            &mut read,
            None,
        )?;
    }
    Ok(())
}

#[cfg(not(windows))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("test console is available on Windows only")
}
