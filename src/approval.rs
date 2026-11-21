use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use crate::terminal::{self, Palette, Tone};
use anyhow::Result;

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

const OPTIONS: [(&str, Decision); 3] = [
    (
        "Always Allow (this exact action, for this session)",
        Decision::Always,
    ),
    ("Allow This Time", Decision::Once),
    ("Deny", Decision::Denied),
];
const DEFAULT_CHOICE: usize = 2;

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
        let mut input = terminal::TerminalInput::new(tty.try_clone()?);
        let palette = Palette::detect(true);
        write_preview(&mut tty, &action.label(), preview, palette)?;
        let selected = terminal::select(
            &mut input,
            &mut tty,
            &OPTIONS.map(|(label, _)| label),
            DEFAULT_CHOICE,
            palette,
        )?;
        crate::tools::check_interrupt()?;
        Ok(self.record(action, decision_for(selected)))
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

fn decision_for(selected: Option<usize>) -> Decision {
    OPTIONS[selected.unwrap_or(DEFAULT_CHOICE)].1
}

fn write_preview(
    output: &mut impl Write,
    label: &str,
    preview: &str,
    palette: Palette,
) -> Result<()> {
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
    Ok(())
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
        for (selection, decision) in [
            (Some(0), Decision::Always),
            (Some(1), Decision::Once),
            (Some(DEFAULT_CHOICE), Decision::Denied),
            (None, Decision::Denied),
        ] {
            let result = decision_for(selection);
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
