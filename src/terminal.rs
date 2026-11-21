use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

use anyhow::Result;
use rustix::event::{
    FdSetElement, Timespec, fd_set_insert, fd_set_num_elements, select as select_io,
};
use rustix::termios::{
    LocalModes, OptionalActions, SpecialCodeIndex, Termios, tcgetattr, tcgetwinsize, tcsetattr,
};

#[derive(Clone, Copy)]
pub enum Tone {
    Info,
    Text,
    Action,
}

#[derive(Clone, Copy, Default)]
pub struct Palette(Option<[&'static str; 3]>);

impl Palette {
    pub fn stderr() -> Self {
        Self::detect(io::stderr().is_terminal())
    }

    pub fn detect(is_terminal: bool) -> Self {
        let term = std::env::var("TERM").unwrap_or_default();
        if !is_terminal
            || term == "dumb"
            || std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty())
        {
            return Self::default();
        }

        // The blue, lavender, and rose accents are from Adaption's homepage stylesheet.
        let color_term = std::env::var("COLORTERM").unwrap_or_default();
        Self(Some(
            if color_term == "truecolor" || color_term == "24bit" || term.ends_with("-direct") {
                [
                    "\x1b[38;2;73;134;180m",
                    "\x1b[38;2;169;186;209m",
                    "\x1b[38;2;218;167;210m",
                ]
            } else if term.contains("256color") {
                ["\x1b[38;5;67m", "\x1b[38;5;146m", "\x1b[38;5;182m"]
            } else {
                ["\x1b[34m", "\x1b[36m", "\x1b[35m"]
            },
        ))
    }

    pub fn paint(self, tone: Tone, text: impl std::fmt::Display) -> String {
        let color = self.0.map(|colors| colors[tone as usize]).unwrap_or("");
        let reset = if color.is_empty() { "" } else { "\x1b[39m" };
        format!("{color}{text}{reset}")
    }
}

pub fn notice(tone: Tone, message: impl std::fmt::Display) {
    eprintln!("{}", Palette::stderr().paint(tone, message));
}

#[derive(Debug)]
pub struct ExitRequested;

impl std::fmt::Display for ExitRequested {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("exit requested")
    }
}

impl std::error::Error for ExitRequested {}

#[derive(Debug)]
pub struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, output: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        output.write_str("interrupted")
    }
}

impl std::error::Error for Interrupted {}

pub fn read_interactive_line(input: &mut (impl BufRead + AsFd)) -> Result<Option<String>> {
    wait_for_input(input.as_fd())?;
    read_prompt_line(input)
}

fn read_prompt_line(input: &mut impl BufRead) -> Result<Option<String>> {
    let result = read_line(input)?;
    if result
        .as_deref()
        .is_some_and(|line| matches!(line.trim(), "quit" | "exit" | "/quit" | "/exit"))
    {
        return Err(ExitRequested.into());
    }
    Ok(result)
}

pub struct TerminalInput(BufReader<File>);

impl TerminalInput {
    pub fn new(file: File) -> Self {
        // Keep each line in the terminal queue until poll observes it.
        Self(BufReader::with_capacity(1, file))
    }
}

impl AsFd for TerminalInput {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.get_ref().as_fd()
    }
}

impl Read for TerminalInput {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl BufRead for TerminalInput {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        self.0.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.0.consume(count);
    }
}

fn wait_for_input(fd: BorrowedFd<'_>) -> Result<()> {
    while !input_ready(fd)? {}
    Ok(())
}

fn input_ready(fd: BorrowedFd<'_>) -> Result<bool> {
    let raw = fd.as_raw_fd();
    let mut readable = vec![FdSetElement::default(); fd_set_num_elements(1, raw + 1)];
    let interval = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    loop {
        crate::tools::check_interrupt()?;
        readable.fill(FdSetElement::default());
        fd_set_insert(&mut readable, raw);
        // macOS poll reports POLLNVAL for /dev/tty.
        // SAFETY: The set contains only the borrowed, open descriptor.
        match unsafe { select_io(raw + 1, Some(&mut readable), None, None, Some(&interval)) } {
            Err(rustix::io::Errno::INTR) => continue,
            Ok(count) => {
                crate::tools::check_interrupt()?;
                return Ok(count != 0);
            }
            Err(error) => return Err(error.into()),
        }
    }
}

struct RestoreTerminal<'a> {
    fd: BorrowedFd<'a>,
    original: Termios,
}

impl Drop for RestoreTerminal<'_> {
    fn drop(&mut self) {
        let _ = tcsetattr(self.fd, OptionalActions::Flush, &self.original);
    }
}

pub fn select(
    input: &mut TerminalInput,
    output: &mut impl Write,
    options: &[impl AsRef<str>],
    initial: usize,
    palette: Palette,
) -> Result<Option<usize>> {
    let fd = rustix::io::dup(input.as_fd())?;
    let original = tcgetattr(&fd)?;
    let mut mode = original.clone();
    mode.local_modes
        .remove(LocalModes::ICANON | LocalModes::ECHO | LocalModes::ECHONL);
    mode.special_codes[SpecialCodeIndex::VMIN] = 1;
    mode.special_codes[SpecialCodeIndex::VTIME] = 0;
    tcsetattr(&fd, OptionalActions::Flush, &mode)?;
    let restore = RestoreTerminal {
        fd: fd.as_fd(),
        original,
    };
    let ansi = std::env::var("TERM").is_ok_and(|term| !term.is_empty() && term != "dumb");
    if ansi {
        write!(output, "\x1b[?2004h")?;
    }
    let selected = select_option(input, output, options, initial, palette, ansi);
    drop(restore);
    if ansi {
        write!(output, "\x1b[?2004l")?;
        output.flush()?;
    }
    selected
}

fn select_option(
    input: &mut TerminalInput,
    output: &mut impl Write,
    options: &[impl AsRef<str>],
    mut selected: usize,
    palette: Palette,
    ansi: bool,
) -> Result<Option<usize>> {
    let size = tcgetwinsize(&*input).ok();
    let rows = size.map_or(0, |size| size.ws_row);
    let count = if rows == 0 {
        9
    } else {
        usize::from(rows.saturating_sub(3)).clamp(1, 9)
    }
    .min(options.len());
    let mut top = 0;
    let mut typed = Vec::with_capacity(6);
    let mut redraw = false;
    loop {
        top = top.min(selected).max((selected + 1).saturating_sub(count));
        if redraw && ansi {
            write!(output, "\x1b[{}F\x1b[J", count + 1)?;
        }
        let width = tcgetwinsize(&*input)
            .ok()
            .map(|size| usize::from(size.ws_col))
            .filter(|&width| width != 0)
            .unwrap_or(80);
        let border = palette.paint(Tone::Info, "│");
        for (offset, option) in options[top..top + count].iter().enumerate() {
            let label = line(option.as_ref());
            let marker = if top + offset == selected {
                "› "
            } else {
                "  "
            };
            let tone = if top + offset == selected {
                Tone::Action
            } else {
                Tone::Text
            };
            writeln!(
                output,
                "{border} {}",
                palette.paint(
                    tone,
                    format!("{marker}{}", fit(&label, width.saturating_sub(5)))
                )
            )?;
        }
        let footer = if count == options.len() {
            "↑/↓ move   Enter select   Esc back".to_owned()
        } else {
            format!(
                "↑/↓ move   Enter select   Esc back   {}/{}",
                selected + 1,
                options.len()
            )
        };
        writeln!(
            output,
            "{}",
            palette.paint(
                Tone::Info,
                format!("╰─ {}", fit(&footer, width.saturating_sub(4)))
            )
        )?;
        output.flush()?;
        redraw = true;
        loop {
            wait_for_input(input.as_fd())?;
            match menu_key(input)? {
                MenuKey::Up | MenuKey::Down if !typed.is_empty() => {}
                MenuKey::Up => {
                    selected = selected.saturating_sub(1);
                    break;
                }
                MenuKey::Down => {
                    selected = (selected + 1).min(options.len() - 1);
                    break;
                }
                MenuKey::Select if typed.is_empty() => return Ok(Some(selected)),
                MenuKey::Select => {
                    if matches!(typed.as_slice(), b"quit" | b"exit" | b"/quit" | b"/exit") {
                        return Err(ExitRequested.into());
                    }
                    typed.clear();
                }
                MenuKey::Cancel => return Ok(None),
                MenuKey::Text(byte) if typed.len() < 6 => typed.push(byte),
                MenuKey::Text(_) | MenuKey::Ignore => {}
            }
        }
    }
}

fn fit(text: &str, width: usize) -> &str {
    let mut columns = 0;
    for (index, ch) in text.char_indices() {
        // Budget up to two terminal cells for non-ASCII characters.
        columns += if ch.is_ascii() { 1 } else { 2 };
        if columns > width {
            return &text[..index];
        }
    }
    text
}

enum MenuKey {
    Up,
    Down,
    Select,
    Cancel,
    Text(u8),
    Ignore,
}

fn menu_key(input: &mut TerminalInput) -> Result<MenuKey> {
    let mut byte = [0];
    if input.read(&mut byte)? == 0 {
        return Ok(MenuKey::Cancel);
    }
    match byte[0] {
        b'\r' | b'\n' => return Ok(MenuKey::Select),
        4 => return Ok(MenuKey::Cancel),
        0x1b => {}
        other => return Ok(MenuKey::Text(other)),
    }
    if !input_ready(input.as_fd())? || input.read(&mut byte)? == 0 {
        return Ok(MenuKey::Cancel);
    }
    if !matches!(byte[0], b'[' | b'O') {
        return Ok(MenuKey::Ignore);
    }
    let mut sequence = Vec::with_capacity(4);
    while input_ready(input.as_fd())? && input.read(&mut byte)? != 0 {
        if sequence.len() < 5 {
            sequence.push(byte[0]);
        }
        if (0x40..=0x7e).contains(&byte[0]) {
            return Ok(match sequence.as_slice() {
                b"A" => MenuKey::Up,
                b"B" => MenuKey::Down,
                b"200~" => {
                    // Bracketed paste is data even if it contains arrow keys or Enter.
                    let end = b"\x1b[201~";
                    let mut matched = 0;
                    while matched < end.len() {
                        wait_for_input(input.as_fd())?;
                        if input.read(&mut byte)? == 0 {
                            return Ok(MenuKey::Cancel);
                        }
                        matched = if byte[0] == end[matched] {
                            matched + 1
                        } else {
                            usize::from(byte[0] == end[0])
                        };
                    }
                    MenuKey::Ignore
                }
                _ => MenuKey::Ignore,
            });
        }
    }
    Ok(MenuKey::Ignore)
}

pub fn read_line(input: &mut impl BufRead) -> io::Result<Option<String>> {
    const MAX_LINE: u64 = 32 * 1024;
    let mut line = String::new();
    let count = input.take(MAX_LINE + 1).read_line(&mut line)?;
    if count as u64 > MAX_LINE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "input line is too long",
        ));
    }
    if count == 0 {
        return Ok(None);
    }
    if line.ends_with('\n') {
        line.pop();
        if line.ends_with('\r') {
            line.pop();
        }
    }
    Ok(Some(line))
}

pub fn read_hidden_line(
    input: &mut (impl BufRead + AsFd),
    output: &mut impl Write,
    prompt: &str,
) -> Result<Option<String>> {
    let fd = rustix::io::dup(input.as_fd())?;
    let original = tcgetattr(&fd)?;
    let mut hidden = original.clone();
    hidden
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ECHONL);
    tcsetattr(&fd, OptionalActions::Flush, &hidden)?;
    let restore = RestoreTerminal {
        fd: fd.as_fd(),
        original,
    };
    write!(output, "{prompt}")?;
    output.flush()?;

    let value = wait_for_input(fd.as_fd()).and_then(|()| read_prompt_line(input));
    drop(restore);
    writeln!(output)?;
    value
}

pub fn visible(text: &str) -> String {
    escape(text, true)
}

pub fn line(text: &str) -> String {
    escape(text, false)
}

fn escape(text: &str, multiline: bool) -> String {
    use std::fmt::Write;

    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\n' | '\t' if multiline => escaped.push(c),
            c if c.is_control()
                || matches!(c, '\u{061c}' | '\u{200b}'..='\u{200f}' | '\u{2028}'..='\u{202e}'
                    | '\u{2060}'..='\u{206f}' | '\u{feff}') =>
            {
                let _ = write!(escaped, "{}", c.escape_unicode());
            }
            _ => escaped.push(c),
        }
    }
    escaped
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_for_input_on_the_controlling_terminal() {
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};

        use rustix::fs::{Mode, OFlags, open};
        use rustix::process::{ioctl_tiocsctty, setsid};
        use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};

        const FIXTURE: &str = "ADAPT_TEST_CONTROLLING_TERMINAL";
        if std::env::var_os(FIXTURE).is_some() {
            let tty = File::open("/dev/tty").unwrap();
            assert!(
                !input_ready(tty.as_fd()).unwrap(),
                "empty terminal was reported ready"
            );
            return;
        }

        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).unwrap();
        grantpt(&master).unwrap();
        unlockpt(&master).unwrap();
        let name = ptsname(&master, Vec::new()).unwrap();
        let slave = open(
            name.as_c_str(),
            OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .unwrap();
        let mut test = Command::new(std::env::current_exe().unwrap());
        test.args([
            "--exact",
            "terminal::tests::waits_for_input_on_the_controlling_terminal",
            "--nocapture",
        ])
        .env(FIXTURE, "1")
        .stdin(Stdio::null());
        // The child creates a new session and takes ownership of the fixture terminal.
        unsafe {
            test.pre_exec(move || {
                setsid()?;
                Ok(ioctl_tiocsctty(&slave)?)
            });
        }
        let result = test.output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    #[test]
    fn escapes_controls_and_bidi_marks_in_untrusted_text() {
        let input = "café\t\n\x1b[0m\r\u{202e}text";
        assert_eq!(visible(input), "café\t\n\\u{1b}[0m\\u{d}\\u{202e}text");
        assert_eq!(line(input), "café\\u{9}\\u{a}\\u{1b}[0m\\u{d}\\u{202e}text");
    }
}
