use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use anyhow::Result;
use rustix::termios::{QueueSelector, tcflush};

use crate::terminal::{self, Palette, Tone};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Command {
        executable: PathBuf,
        argv0: String,
        fingerprint: [u8; 32],
        args: Vec<String>,
        cwd: PathBuf,
        directory_id: (u64, u64),
    },
    Edit {
        path: PathBuf,
        fingerprint: [u8; 32],
    },
    Directory {
        path: PathBuf,
        parent_id: (u64, u64),
    },
}

#[derive(Default)]
pub struct Approvals {
    allowed: Vec<Action>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Decision {
    Always,
    Once,
    Denied,
}

impl Action {
    fn label(&self) -> String {
        match self {
            Self::Command {
                executable,
                argv0,
                args,
                cwd,
                ..
            } => {
                let argv = serde_json::to_string(args).expect("serializing command arguments");
                let mut label = format!(
                    "Run {:?} with arguments {} in {:?}",
                    executable,
                    terminal::line(&argv),
                    cwd
                );
                if executable.as_os_str() != argv0.as_str() {
                    label.push_str(&format!(" (invoked as {argv0:?})"));
                }
                label
            }
            Self::Edit { path, .. } => format!("Edit {path:?}"),
            Self::Directory { path, .. } => format!("Create directory {path:?}"),
        }
    }
}

impl Approvals {
    pub fn request(&mut self, action: Action, preview: &str) -> Result<bool> {
        if self.allowed.contains(&action) {
            terminal::notice(
                Tone::Info,
                format_args!(
                    "Allowed for this session: {}",
                    terminal::line(&action.label())
                ),
            );
            return Ok(true);
        }
        let Ok(mut tty) = OpenOptions::new().read(true).write(true).open("/dev/tty") else {
            terminal::notice(
                Tone::Action,
                format_args!(
                    "Denied: no controlling terminal for {}",
                    terminal::line(&action.label())
                ),
            );
            return Ok(false);
        };
        tcflush(&tty, QueueSelector::IFlush)?;
        let mut input = terminal::TerminalInput::new(tty.try_clone()?);
        let decision = prompt(
            &mut tty,
            &action.label(),
            preview,
            Palette::detect(true),
            || terminal::read_interactive_line(&mut input),
        )?;
        crate::tools::check_interrupt()?;
        Ok(self.record(action, decision))
    }

    fn record(&mut self, action: Action, decision: Decision) -> bool {
        if decision == Decision::Always && !self.allowed.contains(&action) {
            self.allowed.push(action);
        }
        decision != Decision::Denied
    }

    pub fn show(&self) {
        if self.allowed.is_empty() {
            terminal::notice(Tone::Info, "No remembered approvals.");
        }
        for action in &self.allowed {
            eprintln!("  {}", terminal::line(&action.label()));
        }
    }

    pub fn clear(&mut self) {
        self.allowed.clear();
    }
}

fn prompt(
    output: &mut impl Write,
    label: &str,
    preview: &str,
    palette: Palette,
    mut read_choice: impl FnMut() -> Result<Option<String>>,
) -> Result<Decision> {
    let border = palette.paint(Tone::Info, "│");
    writeln!(
        output,
        "\n{} {}",
        palette.paint(Tone::Info, "╭─"),
        palette.paint(Tone::Action, "Approval required")
    )?;
    writeln!(
        output,
        "{border} {}",
        palette.paint(Tone::Text, terminal::line(label))
    )?;
    for line in preview.lines() {
        let text = terminal::visible(line);
        let text = if line.starts_with('+') {
            palette.paint(Tone::Info, text)
        } else if line.starts_with('-') {
            palette.paint(Tone::Action, text)
        } else {
            text
        };
        writeln!(output, "{border} {text}")?;
    }
    writeln!(
        output,
        "{border} {}",
        palette.paint(
            Tone::Text,
            "1. Always Allow (this exact action, for this session)"
        )
    )?;
    writeln!(
        output,
        "{border} {}",
        palette.paint(Tone::Text, "2. Allow This Time")
    )?;
    writeln!(
        output,
        "{border} {}",
        palette.paint(Tone::Action, "3. Deny")
    )?;
    writeln!(output, "{}", palette.paint(Tone::Info, "╰─"))?;
    loop {
        write!(output, "{}", palette.paint(Tone::Action, "Choice [3]: "))?;
        output.flush()?;
        match read_choice()?.as_deref().map(str::trim) {
            Some("1") => return Ok(Decision::Always),
            Some("2") => return Ok(Decision::Once),
            Some("3" | "") | None => return Ok(Decision::Denied),
            _ => writeln!(
                output,
                "{}",
                palette.paint(Tone::Action, "Enter 1, 2, or 3.")
            )?,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn command(args: &[&str]) -> Action {
        Action::Command {
            executable: "/usr/bin/git".into(),
            argv0: "git".into(),
            fingerprint: [4; 32],
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            cwd: "/workspace".into(),
            directory_id: (1, 2),
        }
    }

    #[test]
    fn approvals_match_literal_arguments_and_identity() {
        let mut approvals = Approvals::default();
        let action = command(&["diff", "--check"]);
        assert!(approvals.record(action.clone(), Decision::Always));
        assert!(approvals.allowed.contains(&action));
        assert!(!approvals.allowed.contains(&command(&["diff --check"])));
        assert!(
            !approvals
                .allowed
                .contains(&command(&["diff", "--check", "--cached"]))
        );
        let mut changed_name = action.clone();
        if let Action::Command { argv0, .. } = &mut changed_name {
            *argv0 = "/usr/bin/git".into();
        }
        assert!(!approvals.allowed.contains(&changed_name));
        let mut changed = action.clone();
        if let Action::Command { fingerprint, .. } = &mut changed {
            *fingerprint = [9; 32];
        }
        assert!(!approvals.allowed.contains(&changed));
        if let Action::Command { cwd, .. } = &mut changed {
            *cwd = "/elsewhere".into();
        }
        assert!(!approvals.allowed.contains(&changed));
        approvals.clear();
        assert!(!approvals.allowed.contains(&action));
    }

    #[test]
    fn only_explicit_always_is_remembered() {
        for (input, decision) in [
            ("1\n", Decision::Always),
            ("2\n", Decision::Once),
            ("3\n", Decision::Denied),
            ("\n", Decision::Denied),
            ("", Decision::Denied),
            ("yes\n", Decision::Denied),
        ] {
            let mut output = Vec::new();
            let mut input = input.as_bytes();
            let result = prompt(&mut output, "label", "preview", Palette::default(), || {
                Ok(terminal::read_line(&mut input)?)
            })
            .unwrap();
            assert_eq!(result, decision);
            let mut approvals = Approvals::default();
            let action = command(&["status"]);
            assert_eq!(
                approvals.record(action.clone(), result),
                decision != Decision::Denied
            );
            assert_eq!(
                approvals.allowed.contains(&action),
                decision == Decision::Always
            );
        }
    }
}
