use std::io::{self, BufRead, Read, Write};
use std::os::fd::{AsFd, BorrowedFd};

use anyhow::Result;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::termios::{LocalModes, OptionalActions, Termios, tcgetattr, tcsetattr};

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

    let mut events = [PollFd::new(&fd, PollFlags::IN)];
    let interval = Timespec {
        tv_sec: 0,
        tv_nsec: 100_000_000,
    };
    let value = loop {
        if crate::tools::check_interrupt().is_err() {
            break None;
        }
        match poll(&mut events, Some(&interval)) {
            Ok(0) | Err(rustix::io::Errno::INTR) => continue,
            Ok(_) => break read_line(input)?,
            Err(error) => return Err(error.into()),
        }
    };
    drop(restore);
    writeln!(output)?;
    Ok(value)
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
