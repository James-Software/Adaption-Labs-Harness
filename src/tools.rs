use std::env;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::approval::{Action, Approvals};
use crate::{terminal, workspace::Workspace};

const MAX_CAPTURE: u64 = 8 * 1024;
const MAX_CAPTURE_ON_DISK: u64 = 4 * 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(120);
static INTERRUPTED: AtomicBool = AtomicBool::new(false);

pub fn install_interrupt_handler() -> Result<()> {
    ctrlc::set_handler(|| INTERRUPTED.store(true, Ordering::SeqCst))?;
    Ok(())
}

pub fn reset_interrupt() {
    INTERRUPTED.store(false, Ordering::SeqCst);
}

pub fn check_interrupt() -> Result<()> {
    ensure!(!INTERRUPTED.load(Ordering::SeqCst), "interrupted");
    Ok(())
}

fn spec(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type": "function", "name": name, "description": description,
        "parameters": {"type": "object", "properties": properties, "required": required,
            "additionalProperties": false}})
}

pub fn definitions() -> Vec<Value> {
    let path = json!({"type":"string", "description":"Workspace-relative path. Use . for the workspace root."});
    vec![
        spec(
            "list_files",
            "List project files, honoring ignore rules.",
            json!({
                "path": path, "depth":{"type":"integer","minimum":1,"maximum":8}
            }),
            &[],
        ),
        spec(
            "search_files",
            "Find a literal string in project files, honoring ignore rules.",
            json!({
                "path": path, "query":{"type":"string"}, "case_sensitive":{"type":"boolean"}
            }),
            &["query"],
        ),
        spec(
            "read_file",
            "Read up to 200 lines of a UTF-8 file.",
            json!({
                "path": path, "start_line":{"type":"integer","minimum":1},
                "end_line":{"type":"integer","minimum":1}
            }),
            &["path"],
        ),
        spec(
            "edit_file",
            "Review and apply one text change. Supply exact, unique old_text or null to create a file. Parent directories must exist.",
            json!({
                "path": path, "old_text":{"type":["string","null"]}, "new_text":{"type":"string"}
            }),
            &["path", "old_text", "new_text"],
        ),
        spec(
            "create_directory",
            "Review and create one project directory under an existing parent.",
            json!({
                "path": path
            }),
            &["path"],
        ),
        spec(
            "run_command",
            "Review and execute a developer command directly. program is an executable name or path; args contains exact arguments. Shell syntax is not interpreted.",
            json!({
                "program":{"type":"string"}, "args":{"type":"array","items":{"type":"string"}},
                "cwd":path
            }),
            &["program", "args"],
        ),
    ]
}

fn root_path() -> String {
    ".".to_owned()
}
fn default_depth() -> usize {
    3
}
fn first_line() -> usize {
    1
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "root_path")]
    path: String,
    #[serde(default = "default_depth")]
    depth: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    #[serde(default = "root_path")]
    path: String,
    query: String,
    #[serde(default)]
    case_sensitive: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadArgs {
    path: String,
    #[serde(default = "first_line")]
    start_line: usize,
    end_line: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditArgs {
    path: String,
    #[serde(deserialize_with = "nullable_text")]
    old_text: Option<String>,
    new_text: String,
}

fn nullable_text<'de, D: serde::Deserializer<'de>>(value: D) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(value)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArg {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandArgs {
    program: String,
    args: Vec<String>,
    #[serde(default = "root_path")]
    cwd: String,
}

pub fn run(name: &str, args: &str, ws: &Workspace, approvals: &mut Approvals) -> Value {
    eprintln!("[{}]", terminal::line(name));
    match execute(name, args, ws, approvals) {
        Ok(result) => result,
        Err(error) => json!({"status": "error", "message": format!("{error:#}")}),
    }
}

fn execute(name: &str, args: &str, ws: &Workspace, approvals: &mut Approvals) -> Result<Value> {
    match name {
        "list_files" => {
            let args: ListArgs = serde_json::from_str(args)?;
            Ok(json!({"status":"ok","output":ws.list(&args.path, args.depth)?}))
        }
        "search_files" => {
            let args: SearchArgs = serde_json::from_str(args)?;
            Ok(
                json!({"status":"ok","output":ws.search(&args.path, &args.query, args.case_sensitive)?}),
            )
        }
        "read_file" => {
            let args: ReadArgs = serde_json::from_str(args)?;
            let end = args
                .end_line
                .unwrap_or_else(|| args.start_line.saturating_add(199));
            Ok(json!({"status":"ok","output":ws.read(&args.path, args.start_line, end)?}))
        }
        "edit_file" => {
            let args: EditArgs = serde_json::from_str(args)?;
            let edit = ws.prepare_edit(&args.path, args.old_text.as_deref(), &args.new_text)?;
            let action = Action::Edit {
                path: edit.path().to_owned(),
                fingerprint: edit.identity(),
            };
            if !approvals.request(action, edit.preview())? {
                return Ok(denied());
            }
            check_interrupt()?;
            ws.apply(&edit)?;
            Ok(json!({"status":"ok","path":edit.path()}))
        }
        "create_directory" => {
            let args: PathArg = serde_json::from_str(args)?;
            let directory = ws.prepare_directory(&args.path)?;
            let action = Action::Directory {
                path: directory.path().to_owned(),
                parent_id: directory.parent_id(),
            };
            if !approvals.request(action, "")? {
                return Ok(denied());
            }
            check_interrupt()?;
            ws.create_directory(&directory)?;
            Ok(json!({"status":"ok","path":directory.path()}))
        }
        "run_command" => {
            let args: CommandArgs = serde_json::from_str(args)?;
            command(args, ws, approvals)
        }
        _ => bail!("unknown local tool {name:?}"),
    }
}

fn denied() -> Value {
    json!({"status":"denied","message":"User did not authorize this action."})
}

fn command(args: CommandArgs, ws: &Workspace, approvals: &mut Approvals) -> Result<Value> {
    ensure!(
        args.args.len() <= 64
            && args
                .args
                .iter()
                .all(|arg| arg.len() <= 4096 && !arg.contains('\0')),
        "invalid command arguments"
    );
    let cwd = ws.command_dir(&args.cwd)?;
    let executable = resolve(&args.program, &cwd)?;
    let fingerprint = executable_id(&executable)?;
    let directory_id = directory_id(&cwd)?;
    let action = Action::Command {
        executable: executable.clone(),
        argv0: args.program.clone(),
        args: args.args.clone(),
        cwd: cwd.clone(),
        fingerprint,
        directory_id,
    };
    if !approvals.request(
        action,
        "Runs with your user account. Project scripts and dependencies can change between runs.",
    )? {
        return Ok(denied());
    }
    check_interrupt()?;
    ensure!(
        ws.command_dir(&args.cwd)? == cwd
            && self::directory_id(&cwd)? == directory_id
            && executable_id(&executable)? == fingerprint,
        "executable or working directory changed after review"
    );
    spawn(&executable, &args.program, &args.args, &cwd)
}

fn resolve(program: &str, cwd: &Path) -> Result<PathBuf> {
    ensure!(
        !program.is_empty() && program.len() <= 4096 && terminal::line(program) == program,
        "provide an executable name or path"
    );
    let path = Path::new(program);
    let resolved = if path.is_absolute() {
        path.to_owned()
    } else if program.contains('/') {
        cwd.join(path)
    } else {
        let search = env::var_os("PATH").context("PATH is unset")?;
        env::split_paths(&search)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(path))
            .find(|candidate| candidate.metadata().is_ok_and(is_executable))
            .with_context(|| format!("executable {program:?} was not found on PATH"))?
    };
    let absolute = resolved
        .canonicalize()
        .context("resolving the executable")?;
    ensure!(
        is_executable(absolute.metadata()?),
        "expected a regular executable file"
    );
    Ok(absolute)
}

fn is_executable(metadata: fs::Metadata) -> bool {
    metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
}

fn executable_id(path: &Path) -> Result<[u8; 32]> {
    let file = File::from(rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )?);
    let meta = file.metadata()?;
    const MAX_EXECUTABLE: u64 = 512 * 1024 * 1024;
    ensure!(
        is_executable(meta.clone()) && meta.len() <= MAX_EXECUTABLE,
        "expected a regular executable up to 512 MiB"
    );
    let mut hash = Sha256::new();
    for part in [meta.dev(), meta.ino(), meta.mode() as u64] {
        hash.update(part.to_le_bytes());
    }
    let count = io::copy(&mut (&file).take(MAX_EXECUTABLE + 1), &mut hash)?;
    let after = file.metadata()?;
    ensure!(
        count == meta.len()
            && (meta.ctime(), meta.ctime_nsec()) == (after.ctime(), after.ctime_nsec()),
        "executable changed during review"
    );
    Ok(hash.finalize().into())
}

fn directory_id(path: &Path) -> Result<(u64, u64)> {
    let meta = path.metadata()?;
    ensure!(meta.is_dir(), "working directory is no longer a directory");
    Ok((meta.dev(), meta.ino()))
}

fn spawn(program: &Path, argv0: &str, args: &[String], cwd: &Path) -> Result<Value> {
    let mut stdout = tempfile::tempfile()?;
    let mut stderr = tempfile::tempfile()?;
    let mut child = Command::new(program)
        .arg0(argv0)
        .args(args)
        .current_dir(cwd)
        .env_remove("ADAPTION_API_KEY")
        .stdin(Stdio::null())
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?)
        .process_group(0)
        .spawn()
        .context("starting command")?;
    let group = Pid::from_child(&child);
    let start = Instant::now();
    let reason = loop {
        match waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOWAIT | WaitIdOptions::NOHANG,
        ) {
            Ok(Some(_)) => break None,
            Ok(None) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        let reason = if INTERRUPTED.load(Ordering::SeqCst) {
            Some("interrupted")
        } else if start.elapsed() >= TIMEOUT {
            Some("command timed out after 120 seconds")
        } else if [&stdout, &stderr].iter().any(|file| {
            file.metadata()
                .is_ok_and(|meta| meta.len() > MAX_CAPTURE_ON_DISK)
        }) {
            Some("command exceeded the output limit")
        } else {
            None
        };
        if reason.is_some() {
            break reason;
        }
        thread::sleep(Duration::from_millis(40));
    };
    // Keep the leader unreaped until cleanup so its process-group ID cannot be reused.
    let _ = kill_process_group(group, Signal::KILL);
    let status = child.wait()?;
    let stdout = capture(&mut stdout)?;
    let stderr = capture(&mut stderr)?;
    if !stdout.is_empty() {
        eprintln!("{}", terminal::visible(&stdout));
    }
    if !stderr.is_empty() {
        eprintln!("{}", terminal::visible(&stderr));
    }
    eprintln!(
        "Command: {}",
        reason.map_or_else(|| status.to_string(), str::to_owned)
    );
    Ok(json!({
        "status": if reason.is_none() && status.success() { "ok" } else { "error" },
        "exit_code": status.code(), "reason": reason, "stdout": stdout, "stderr": stderr
    }))
}

fn capture(file: &mut File) -> Result<String> {
    file.seek(SeekFrom::Start(0))?;
    let mut bytes = Vec::new();
    file.take(MAX_CAPTURE + 1).read_to_end(&mut bytes)?;
    let truncated = bytes.len() as u64 > MAX_CAPTURE;
    bytes.truncate(MAX_CAPTURE as usize);
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if truncated {
        output.push_str("\n[output truncated]");
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_runs_literal_argv() {
        let root = tempfile::tempdir().unwrap();
        let result = spawn(
            Path::new("/usr/bin/printf"),
            "printf",
            &["%s".into(), "$(touch marker); *".into()],
            root.path(),
        )
        .unwrap();
        assert_eq!(result["status"], "ok");
        assert_eq!(result["stdout"], "$(touch marker); *");
        assert!(!root.path().join("marker").exists());
    }

    #[test]
    fn edit_requires_explicit_old_text() {
        assert!(serde_json::from_str::<EditArgs>(r#"{"path":"x","new_text":"x"}"#).is_err());
        assert!(
            serde_json::from_str::<EditArgs>(r#"{"path":"x","old_text":null,"new_text":"x"}"#)
                .is_ok()
        );
    }
}
