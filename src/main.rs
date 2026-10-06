use clap::{Parser, ValueEnum};
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(version, about = "Send text to the Windows clipboard")]
struct Arguments {
    #[arg(
        value_name = "FILE",
        help = "Read from a file, or standard input if omitted or '-'"
    )]
    file: Option<PathBuf>,
    #[arg(
        long,
        value_enum,
        default_value = "windows",
        help = "Normalise line endings to CRLF or preserve them"
    )]
    line_endings: LineEndings,
    #[arg(
        long,
        help = "Write UTF-8 text to standard output without changing the clipboard"
    )]
    preview: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum LineEndings {
    Windows,
    Preserve,
}

fn read_input(file: Option<&Path>, input: &mut impl Read) -> Result<Vec<u8>, String> {
    if let Some(path) = file.filter(|path| *path != Path::new("-")) {
        return fs::read(path)
            .map_err(|error| format!("could not read '{}': {error}", path.display()));
    }
    let mut bytes = Vec::new();
    input
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read standard input: {error}"))?;
    Ok(bytes)
}

fn decode_input(bytes: &[u8]) -> Result<String, String> {
    let utf16 = if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        Some((body, true, "UTF-16LE"))
    } else {
        bytes
            .strip_prefix(&[0xfe, 0xff])
            .map(|body| (body, false, "UTF-16BE"))
    };
    if let Some((body, little_endian, encoding)) = utf16 {
        if !body.len().is_multiple_of(2) {
            return Err(format!("input contains an incomplete {encoding} code unit"));
        }
        let units = body.chunks_exact(2).map(|pair| {
            let pair = [pair[0], pair[1]];
            if little_endian {
                u16::from_le_bytes(pair)
            } else {
                u16::from_be_bytes(pair)
            }
        });
        return char::decode_utf16(units)
            .collect::<Result<String, _>>()
            .map_err(|error| {
                format!(
                    "input contains an unpaired {encoding} surrogate U+{:04X}",
                    error.unpaired_surrogate()
                )
            });
    }
    let body = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
    std::str::from_utf8(body)
        .map(str::to_owned)
        .map_err(|error| format!("input is not valid UTF-8: {error}"))
}

fn normalise_line_endings(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '\r' => {
                if characters.peek() == Some(&'\n') {
                    characters.next();
                }
                output.push_str("\r\n");
            }
            '\n' => output.push_str("\r\n"),
            _ => output.push(character),
        }
    }
    output
}

fn prepare_text(bytes: &[u8], line_endings: LineEndings) -> Result<String, String> {
    let text = decode_input(bytes)?;
    if text.contains('\0') {
        return Err("input contains an embedded NUL character".into());
    }
    Ok(match line_endings {
        LineEndings::Windows => normalise_line_endings(&text),
        LineEndings::Preserve => text,
    })
}

#[cfg(windows)]
mod clipboard {
    use std::ffi::c_void;
    use std::{io, mem, ptr};

    type Handle = *mut c_void;

    const CF_UNICODETEXT: u32 = 13;
    const GMEM_MOVEABLE: u32 = 0x0002;
    const HWND_MESSAGE: Handle = -3_isize as Handle;

    #[link(name = "user32")]
    unsafe extern "system" {
        fn CreateWindowExW(
            extended_style: u32,
            class_name: *const u16,
            window_name: *const u16,
            style: u32,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            parent: Handle,
            menu: Handle,
            instance: Handle,
            parameter: *mut c_void,
        ) -> Handle;
        fn DestroyWindow(window: Handle) -> i32;
        fn OpenClipboard(window: Handle) -> i32;
        fn CloseClipboard() -> i32;
        fn EmptyClipboard() -> i32;
        fn SetClipboardData(format: u32, memory: Handle) -> Handle;
    }

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GlobalAlloc(flags: u32, bytes: usize) -> Handle;
        fn GlobalFree(memory: Handle) -> Handle;
        fn GlobalLock(memory: Handle) -> *mut c_void;
        fn GlobalUnlock(memory: Handle) -> i32;
        fn SetLastError(error: u32);
    }

    fn failure(operation: &str) -> String {
        format!("could not {operation}: {}", io::Error::last_os_error())
    }

    struct Window(Handle);

    impl Window {
        fn new() -> Result<Self, String> {
            let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
            let handle = unsafe {
                CreateWindowExW(
                    0,
                    class.as_ptr(),
                    ptr::null(),
                    0,
                    0,
                    0,
                    0,
                    0,
                    HWND_MESSAGE,
                    ptr::null_mut(),
                    ptr::null_mut(),
                    ptr::null_mut(),
                )
            };
            if handle.is_null() {
                Err(failure("create a clipboard owner window"))
            } else {
                Ok(Self(handle))
            }
        }
    }

    impl Drop for Window {
        fn drop(&mut self) {
            unsafe {
                DestroyWindow(self.0);
            }
        }
    }

    struct Clipboard(bool);

    impl Clipboard {
        fn open(window: &Window) -> Result<Self, String> {
            if unsafe { OpenClipboard(window.0) } == 0 {
                Err(failure("open the clipboard"))
            } else {
                Ok(Self(true))
            }
        }

        fn close(&mut self) -> Result<(), String> {
            if unsafe { CloseClipboard() } == 0 {
                return Err(failure("close the clipboard"));
            }
            self.0 = false;
            Ok(())
        }
    }

    impl Drop for Clipboard {
        fn drop(&mut self) {
            if self.0 {
                unsafe {
                    CloseClipboard();
                }
            }
        }
    }

    struct GlobalMemory {
        handle: Handle,
        locked: bool,
    }

    impl GlobalMemory {
        fn new(text: &str) -> Result<Self, String> {
            let units: Vec<u16> = text.encode_utf16().chain([0]).collect();
            let bytes = units
                .len()
                .checked_mul(mem::size_of::<u16>())
                .ok_or("clipboard text is too large")?;
            let handle = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes) };
            if handle.is_null() {
                return Err(failure("allocate clipboard memory"));
            }
            let mut memory = Self {
                handle,
                locked: false,
            };
            let buffer = unsafe { GlobalLock(handle) }.cast::<u16>();
            if buffer.is_null() {
                return Err(failure("lock clipboard memory"));
            }
            memory.locked = true;
            unsafe {
                ptr::copy_nonoverlapping(units.as_ptr(), buffer, units.len());
                SetLastError(0);
                if GlobalUnlock(handle) == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() != Some(0) {
                        return Err(format!("could not unlock clipboard memory: {error}"));
                    }
                }
            }
            memory.locked = false;
            Ok(memory)
        }
    }

    impl Drop for GlobalMemory {
        fn drop(&mut self) {
            if !self.handle.is_null() {
                unsafe {
                    if self.locked {
                        GlobalUnlock(self.handle);
                    }
                    GlobalFree(self.handle);
                }
            }
        }
    }

    pub fn write(text: &str) -> Result<(), String> {
        let mut memory = GlobalMemory::new(text)?;
        let window = Window::new()?;
        let mut clipboard = Clipboard::open(&window)?;
        if unsafe { EmptyClipboard() } == 0 {
            return Err(failure("empty the clipboard"));
        }
        if unsafe { SetClipboardData(CF_UNICODETEXT, memory.handle) }.is_null() {
            return Err(failure("write clipboard text"));
        }
        memory.handle = ptr::null_mut();
        clipboard.close()
    }
}

#[cfg(not(windows))]
mod clipboard {
    pub fn write(_text: &str) -> Result<(), String> {
        Err("clipboard writes are supported only on Windows; use --preview".into())
    }
}

fn run(
    arguments: &Arguments,
    input: &mut impl Read,
    output: &mut impl Write,
) -> Result<(), String> {
    let bytes = read_input(arguments.file.as_deref(), input)?;
    let text = prepare_text(&bytes, arguments.line_endings)?;
    if arguments.preview {
        output
            .write_all(text.as_bytes())
            .and_then(|()| output.flush())
            .map_err(|error| format!("could not write standard output: {error}"))
    } else {
        clipboard::write(&text)
    }
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    match run(
        &arguments,
        &mut io::stdin().lock(),
        &mut io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn normalises_mixed_and_consecutive_line_endings() {
        for (input, expected) in [
            (
                "alpha\nbeta\rgamma\r\ndelta\n",
                "alpha\r\nbeta\r\ngamma\r\ndelta\r\n",
            ),
            ("\r\r\n\n", "\r\n\r\n\r\n"),
            ("colour\r", "colour\r\n"),
            ("caf\u{e9} \u{1f600}", "caf\u{e9} \u{1f600}"),
            ("", ""),
        ] {
            let actual = normalise_line_endings(input);
            assert_eq!(actual, expected);
            assert_eq!(normalise_line_endings(&actual), expected);
        }
    }

    #[test]
    fn decodes_utf8_and_removes_only_the_initial_bom() {
        assert_eq!(decode_input(b"colour").unwrap(), "colour");
        assert_eq!(decode_input(b"\xef\xbb\xbfcolour").unwrap(), "colour");
        assert_eq!(decode_input(b"\xef\xbb\xbf").unwrap(), "");
        assert_eq!(decode_input("a\u{feff}b".as_bytes()).unwrap(), "a\u{feff}b");
    }

    #[test]
    fn decodes_both_utf16_byte_orders_and_surrogate_pairs() {
        let text = "caf\u{e9}\n\u{1f600}\r\n";
        for little_endian in [true, false] {
            let mut bytes = if little_endian {
                vec![0xff, 0xfe]
            } else {
                vec![0xfe, 0xff]
            };
            assert_eq!(decode_input(&bytes).unwrap(), "");
            for unit in text.encode_utf16() {
                bytes.extend(if little_endian {
                    unit.to_le_bytes()
                } else {
                    unit.to_be_bytes()
                });
            }
            assert_eq!(decode_input(&bytes).unwrap(), text);
        }
    }

    #[test]
    fn rejects_invalid_utf8_and_utf16_sequences() {
        for bytes in [
            &[0xff][..],
            &[0xef, 0xbb],
            &[0xc0, 0xaf],
            &[0xed, 0xa0, 0x80],
            &[0xff, 0xfe, 0x61],
            &[0xfe, 0xff, 0x00],
            &[0xff, 0xfe, 0x00, 0xd8],
            &[0xff, 0xfe, 0x00, 0xdc],
            &[0xfe, 0xff, 0xd8, 0x00, 0x00, 0x61],
            &[0xfe, 0xff, 0xdc, 0x00],
        ] {
            assert!(decode_input(bytes).is_err(), "accepted {bytes:?}");
        }
    }

    #[test]
    fn previews_standard_input_without_a_bom_or_added_newline() {
        for flags in [vec!["clip", "--preview"], vec!["clip", "--preview", "-"]] {
            let arguments = Arguments::try_parse_from(flags).unwrap();
            let mut output = Vec::new();
            run(
                &arguments,
                &mut Cursor::new(b"\xef\xbb\xbfalpha\nbeta"),
                &mut output,
            )
            .unwrap();
            assert_eq!(output, b"alpha\r\nbeta");
        }
        let arguments =
            Arguments::try_parse_from(["clip", "--preview", "--line-endings", "preserve"]).unwrap();
        let mut output = Vec::new();
        run(
            &arguments,
            &mut Cursor::new(b"alpha\nbeta\rgamma\r\ndelta"),
            &mut output,
        )
        .unwrap();
        assert_eq!(output, b"alpha\nbeta\rgamma\r\ndelta");
    }

    #[test]
    fn rejects_embedded_nul_before_writing_any_output() {
        let arguments = Arguments::try_parse_from(["clip", "--preview"]).unwrap();
        for bytes in [
            &b"alpha\0beta\n"[..],
            &[0xff, 0xfe, 0x61, 0x00, 0x00, 0x00],
            &[0xfe, 0xff, 0x00, 0x61, 0x00, 0x00],
        ] {
            for endings in [LineEndings::Windows, LineEndings::Preserve] {
                assert!(prepare_text(bytes, endings).unwrap_err().contains("NUL"));
            }
            let mut output = Vec::new();
            assert!(
                run(&arguments, &mut Cursor::new(bytes), &mut output)
                    .unwrap_err()
                    .contains("NUL")
            );
            assert!(output.is_empty());
        }
    }

    #[test]
    fn reports_input_and_output_failures() {
        struct FailedInput;

        impl Read for FailedInput {
            fn read(&mut self, _buffer: &mut [u8]) -> io::Result<usize> {
                Err(io::Error::other("input unavailable"))
            }
        }

        let arguments = Arguments::try_parse_from(["clip", "--preview"]).unwrap();
        assert!(
            run(&arguments, &mut FailedInput, &mut Vec::new())
                .unwrap_err()
                .contains("could not read standard input")
        );
        assert!(
            run(
                &arguments,
                &mut Cursor::new(b"colour"),
                &mut [0u8; 0].as_mut_slice(),
            )
            .unwrap_err()
            .contains("could not write standard output")
        );
    }

    #[test]
    fn rejects_unknown_line_ending_options() {
        assert!(
            Arguments::try_parse_from(["clip", "--preview", "--line-endings", "unix"]).is_err()
        );
    }

    #[cfg(not(windows))]
    #[test]
    fn reports_unsupported_clipboard_writes() {
        assert!(clipboard::write("colour").unwrap_err().contains("Windows"));
    }
}
