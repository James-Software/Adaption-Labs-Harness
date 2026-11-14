use std::io::{self, BufRead, Read};

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
