use std::fs::File;
use std::io::{self, BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::Result;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};

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
    let mut events = [PollFd::from_borrowed_fd(fd, PollFlags::IN)];
    let interval = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    loop {
        crate::tools::check_interrupt()?;
        match poll(&mut events, Some(&interval)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => return crate::tools::check_interrupt(),
            Err(error) => return Err(error.into()),
        }
    }
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
    struct RestoreEcho<'a> {
        fd: BorrowedFd<'a>,
        original: Termios,
    }

    impl Drop for RestoreEcho<'_> {
        fn drop(&mut self) {
            let _ = tcsetattr(self.fd, OptionalActions::Flush, &self.original);
        }
    }

    let fd = rustix::io::dup(input.as_fd())?;
    let original = tcgetattr(&fd)?;
    let mut hidden = original.clone();
    hidden
        .local_modes
        .remove(LocalModes::ECHO | LocalModes::ECHONL);
    tcsetattr(&fd, OptionalActions::Flush, &hidden)?;
    let restore = RestoreEcho {
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
