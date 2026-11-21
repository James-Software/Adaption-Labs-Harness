use std::env;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, bail, ensure};
use cap_fs_ext::DirExt;
use cap_std::ambient_authority;
use cap_std::fs::{Dir, MetadataExt as CapMetadataExt};
use ignore::WalkBuilder;
use rustix::fs::{Mode, OFlags, openat};
use sha2::{Digest, Sha256};
use similar::TextDiff;

use crate::terminal;

const MAX_FILE: usize = 256 * 1024;
const MAX_EDIT: usize = 64 * 1024;
const MAX_DIFF: usize = 32 * 1024;
const MAX_SCAN: usize = 2_000;
const MAX_SCAN_BYTES: usize = 12 * 1024 * 1024;
const MAX_RESULT: usize = 8 * 1024;
const MAX_LINE_CHARS: usize = 500;
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

pub struct Workspace {
    root: PathBuf,
    dir: Dir,
}

pub struct Edit {
    target: PathBuf,
    parent_id: (u64, u64),
    before: Option<Snapshot>,
    after: String,
    diff: String,
}

pub struct NewDirectory {
    target: PathBuf,
    parent_id: (u64, u64),
}

#[derive(PartialEq, Eq)]
struct Snapshot {
    text: String,
    device: u64,
    inode: u64,
    mode: u32,
    changed: (i64, i64),
}

impl Workspace {
    pub fn open(dir: Option<PathBuf>) -> Result<Self> {
        let root = match dir {
            Some(dir) => dir,
            None => {
                let cwd = env::current_dir()?;
                cwd.ancestors()
                    .find(|dir| dir.join(".git").exists())
                    .unwrap_or(&cwd)
                    .to_owned()
            }
        }
        .canonicalize()
        .context("resolving the workspace")?;
        ensure!(
            root.iter().filter_map(OsStr::to_str).all(allowed),
            "select a project outside protected directories"
        );
        let dir =
            Dir::open_ambient_dir(&root, ambient_authority()).context("opening the workspace")?;
        Ok(Self { root, dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn directory(&self, path: &Path) -> Result<Dir> {
        let mut dir = self.dir.try_clone()?;
        for name in path.iter() {
            dir = dir
                .open_dir_nofollow(name)
                .with_context(|| format!("opening directory {}", path.display()))?;
        }
        Ok(dir)
    }

    pub fn command_dir(&self, raw: &str) -> Result<PathBuf> {
        let path = relative(raw, true)?;
        let dir = self.directory(&path)?;
        let cwd = self.root.join(path);
        ensure!(cwd.canonicalize()? == cwd, "command directory changed");
        let metadata = cwd.metadata()?;
        ensure!(
            directory_id(&dir)? == (metadata.dev(), metadata.ino()),
            "command directory changed"
        );
        Ok(cwd)
    }

    fn parent<'a>(&self, path: &'a Path) -> Result<(Dir, &'a OsStr)> {
        let parent = path
            .parent()
            .context("path must name a file or directory")?;
        Ok((
            self.directory(parent)?,
            path.file_name().context("path needs a name")?,
        ))
    }

    fn snapshot(&self, path: &Path) -> Result<Option<Snapshot>> {
        let (parent, name) = self.parent(path)?;
        read_snapshot(&parent, name)
    }

    pub fn read(&self, raw: &str, start: usize, end: usize) -> Result<String> {
        ensure!(
            start > 0 && end >= start && end - start < 200,
            "request 1 to 200 lines (one-based, inclusive)"
        );
        let path = relative(raw, false)?;
        let file = self.snapshot(&path)?.context("file does not exist")?;
        let mut output = String::new();
        for (index, line) in file
            .text
            .lines()
            .enumerate()
            .skip(start - 1)
            .take(end - start + 1)
        {
            let count = index + 1;
            if !append(&mut output, &format!("{count}: {}", shorten(line))) {
                output.push_str("[result truncated]\n");
                break;
            }
        }
        if output.is_empty() {
            output.push_str("(no lines in the requested range)");
        }
        Ok(output)
    }

    pub fn list(&self, raw: &str, max_depth: usize) -> Result<String> {
        ensure!(
            (1..=8).contains(&max_depth),
            "depth must be between 1 and 8"
        );
        self.walk(raw, max_depth, |path, output| {
            append(output, &path.to_string_lossy())
        })
    }

    pub fn search(&self, raw: &str, query: &str, case_sensitive: bool) -> Result<String> {
        ensure!(
            !query.is_empty() && query.len() <= 256 && !query.contains('\n'),
            "provide a nonempty single-line search string up to 256 bytes"
        );
        let query = if case_sensitive {
            query.to_owned()
        } else {
            query.to_lowercase()
        };
        let mut bytes = 0;
        self.walk(raw, 32, |path, output| {
            let Ok(Some(file)) = self.snapshot(path) else {
                return true;
            };
            bytes += file.text.len();
            if bytes > MAX_SCAN_BYTES {
                return false;
            }
            for (index, line) in file.text.lines().enumerate() {
                let found = if case_sensitive {
                    line.contains(&query)
                } else {
                    line.to_lowercase().contains(&query)
                };
                if found
                    && !append(
                        output,
                        &format!("{}:{}: {}", path.display(), index + 1, shorten(line)),
                    )
                {
                    return false;
                }
            }
            true
        })
    }

    fn walk(
        &self,
        raw: &str,
        depth: usize,
        mut visit: impl FnMut(&Path, &mut String) -> bool,
    ) -> Result<String> {
        let base = relative(raw, true)?;
        self.directory(&base)?;
        let mut output = String::new();
        let entries = WalkBuilder::new(self.root.join(base))
            .max_depth(Some(depth))
            .max_filesize(Some(MAX_FILE as u64))
            .parents(true)
            .require_git(false)
            .follow_links(false)
            .sort_by_file_path(Path::cmp)
            .filter_entry(|entry| {
                entry.depth() == 0 || entry.file_name().to_str().is_some_and(allowed)
            })
            .build();
        for (index, entry) in entries.enumerate() {
            if index >= MAX_SCAN {
                output.push_str("[scan limit reached]\n");
                break;
            }
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            let path = entry.path().strip_prefix(&self.root)?;
            if relative(&path.to_string_lossy(), false).is_ok() && !visit(path, &mut output) {
                output.push_str("[result truncated]\n");
                break;
            }
        }
        if output.is_empty() {
            output.push_str("(no results)");
        }
        Ok(output)
    }

    pub fn prepare_edit(&self, raw: &str, old: Option<&str>, new: &str) -> Result<Edit> {
        let path = relative(raw, false)?;
        ensure!(
            new.len() <= MAX_EDIT && !new.contains('\0'),
            "replacement must be text up to 64 KiB"
        );
        let (parent, name) = self.parent(&path)?;
        let parent_id = directory_id(&parent)?;
        let before = read_snapshot(&parent, name)?;
        let after = match (&before, old) {
            (None, None) => new.to_owned(),
            (Some(_), None) => bail!("file already exists; supply its exact text to replace"),
            (None, Some(_)) => bail!("file does not exist; set old_text to null to create it"),
            (Some(file), Some(old)) => {
                ensure!(
                    old.len() <= MAX_EDIT && (!old.is_empty() || file.text.is_empty()),
                    "old_text may be empty only for an empty file and must be at most 64 KiB"
                );
                let offset = file
                    .text
                    .find(old)
                    .context("old_text is absent; reread the file")?;
                if let Some(first) = old.chars().next() {
                    let next = offset + first.len_utf8();
                    ensure!(
                        !file.text[next..].contains(old),
                        "old_text occurs more than once; supply more surrounding text"
                    );
                }
                let mut after = file.text.clone();
                after.replace_range(offset..offset + old.len(), new);
                ensure!(after != file.text, "edit makes no change");
                after
            }
        };
        ensure!(after.len() <= MAX_FILE, "edited file exceeds 256 KiB");
        let original = before.as_ref().map_or("", |file| file.text.as_str());
        let before_label = before
            .as_ref()
            .map(|_| format!("a/{}", path.display()))
            .unwrap_or_else(|| "/dev/null".into());
        let after_label = format!("b/{}", path.display());
        let diff = TextDiff::configure()
            .timeout(std::time::Duration::from_millis(500))
            .diff_lines(original, &after)
            .unified_diff()
            .header(&before_label, &after_label)
            .to_string();
        let diff = if diff.is_empty() {
            format!("Create empty file {}", path.display())
        } else {
            diff
        };
        let diff = terminal::visible(&diff);
        ensure!(
            diff.len() <= MAX_DIFF,
            "diff exceeds 32 KiB; propose smaller edits"
        );
        Ok(Edit {
            target: path,
            parent_id,
            before,
            after,
            diff,
        })
    }

    pub fn apply(&self, edit: &Edit) -> Result<()> {
        let (parent, name) = self.parent(&edit.target)?;
        ensure!(
            directory_id(&parent)? == edit.parent_id,
            "parent directory changed after preview"
        );
        ensure!(
            read_snapshot(&parent, name)? == edit.before,
            "file changed after preview; read it and review a new edit"
        );
        let temporary = format!(
            ".adaption-edit-{}-{}",
            std::process::id(),
            TEMP_ID.fetch_add(1, Ordering::Relaxed)
        );
        let fd = openat(
            &parent,
            temporary.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::RUSR | Mode::WUSR,
        )?;
        let mut file = File::from(fd);
        let result = (|| {
            file.write_all(edit.after.as_bytes())?;
            if let Some(before) = &edit.before {
                file.set_permissions(std::fs::Permissions::from_mode(before.mode & 0o777))?;
            }
            file.sync_all()?;
            let (current_parent, current_name) = self.parent(&edit.target)?;
            ensure!(
                directory_id(&current_parent)? == edit.parent_id,
                "parent directory changed after preview"
            );
            ensure!(
                read_snapshot(&parent, current_name)? == edit.before,
                "file changed after preview; review a new edit"
            );
            if edit.before.is_some() {
                parent.rename(&temporary, &parent, name)?;
            } else {
                parent.hard_link(&temporary, &parent, name)?;
            }
            Ok(())
        })();
        if let Err(error) = parent.remove_file(&temporary)
            && error.kind() != io::ErrorKind::NotFound
            && result.is_ok()
        {
            eprintln!(
                "Temporary edit cleanup: {}",
                terminal::line(&error.to_string())
            );
        }
        result
    }

    pub fn prepare_directory(&self, raw: &str) -> Result<NewDirectory> {
        let target = relative(raw, false)?;
        let (parent, name) = self.parent(&target)?;
        match parent.symlink_metadata(name) {
            Ok(_) => bail!("path already exists"),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(NewDirectory {
                target,
                parent_id: directory_id(&parent)?,
            }),
            Err(error) => Err(error.into()),
        }
    }

    pub fn create_directory(&self, directory: &NewDirectory) -> Result<()> {
        let (parent, name) = self.parent(&directory.target)?;
        ensure!(
            directory_id(&parent)? == directory.parent_id,
            "parent directory changed after preview"
        );
        parent.create_dir(name)?;
        Ok(())
    }
}

impl Edit {
    pub fn path(&self) -> &Path {
        &self.target
    }
    pub fn preview(&self) -> &str {
        &self.diff
    }

    pub fn identity(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(self.parent_id.0.to_le_bytes());
        hash.update(self.parent_id.1.to_le_bytes());
        if let Some(before) = &self.before {
            hash.update([1]);
            hash.update(before.device.to_le_bytes());
            hash.update(before.inode.to_le_bytes());
            hash.update(before.mode.to_le_bytes());
            hash.update(before.text.len().to_le_bytes());
            hash.update(before.text.as_bytes());
        } else {
            hash.update([0]);
        }
        hash.update(self.after.as_bytes());
        hash.finalize().into()
    }
}

impl NewDirectory {
    pub fn path(&self) -> &Path {
        &self.target
    }

    pub fn parent_id(&self) -> (u64, u64) {
        self.parent_id
    }
}

fn directory_id(dir: &Dir) -> Result<(u64, u64)> {
    let metadata = dir.dir_metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}

fn read_snapshot(parent: &Dir, name: &OsStr) -> Result<Option<Snapshot>> {
    let fd = match openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let file = File::from(fd);
    let meta = file.metadata()?;
    ensure!(
        meta.is_file() && meta.nlink() == 1,
        "expected a regular, single-link file"
    );
    ensure!(meta.len() <= MAX_FILE as u64, "file exceeds 256 KiB");
    let mut bytes = Vec::new();
    (&file).take(MAX_FILE as u64 + 1).read_to_end(&mut bytes)?;
    let after_read = file.metadata()?;
    ensure!(
        (meta.ctime(), meta.ctime_nsec(), meta.len())
            == (
                after_read.ctime(),
                after_read.ctime_nsec(),
                after_read.len()
            ),
        "file changed while being read"
    );
    ensure!(
        bytes.len() <= MAX_FILE && !bytes.contains(&0),
        "expected a text file up to 256 KiB"
    );
    Ok(Some(Snapshot {
        text: String::from_utf8(bytes).context("file is not UTF-8")?,
        device: meta.dev(),
        inode: meta.ino(),
        mode: meta.mode(),
        changed: (meta.ctime(), meta.ctime_nsec()),
    }))
}

fn relative(raw: &str, allow_root: bool) -> Result<PathBuf> {
    ensure!(
        !raw.is_empty() && raw.len() <= 4096 && !raw.contains('\\'),
        "provide a workspace-relative path"
    );
    let mut path = PathBuf::new();
    for part in Path::new(raw).components() {
        match part {
            Component::CurDir => {}
            Component::Normal(name) if name.to_str().is_some_and(allowed) => path.push(name),
            _ => bail!("path is outside the workspace or names a protected file: {raw:?}"),
        }
    }
    ensure!(
        allow_root || !path.as_os_str().is_empty(),
        "path must name a file or directory"
    );
    Ok(path)
}

fn allowed(name: &str) -> bool {
    let name = name.to_lowercase();
    !(terminal::line(&name) != name
        || name == ".git"
        || name == ".ssh"
        || name == ".adaption-code"
        || name == ".aws"
        || name == ".netrc"
        || name == ".npmrc"
        || name == ".pypirc"
        || name == ".env"
        || name.starts_with(".env.")
        || name.starts_with(".adaption-edit-")
        || name.ends_with(".pem")
        || name.ends_with(".key"))
}

fn shorten(line: &str) -> String {
    let mut chars = line.chars();
    let mut output: String = chars.by_ref().take(MAX_LINE_CHARS).collect();
    if chars.next().is_some() {
        output.push_str(" [line truncated]");
    }
    output
}

fn append(output: &mut String, line: &str) -> bool {
    if output.len() + line.len() + 1 > MAX_RESULT {
        return false;
    }
    output.push_str(line);
    output.push('\n');
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;

    #[test]
    fn applies_the_exact_reviewed_edit() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        let path = root.path().join("notes.txt");
        let create = ws
            .prepare_edit("notes.txt", None, "hello\nworld\n")
            .unwrap();
        ws.apply(&create).unwrap();
        let edit = ws
            .prepare_edit("notes.txt", Some("world"), "there")
            .unwrap();
        assert!(edit.preview().contains("-world"));
        assert!(edit.preview().contains("+there"));
        ws.apply(&edit).unwrap();
        assert_eq!(fs::read_to_string(path).unwrap(), "hello\nthere\n");
    }

    #[test]
    fn rejects_stale_creation_and_edit() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        let path = root.path().join("notes.txt");
        let create = ws.prepare_edit("notes.txt", None, "from agent").unwrap();
        fs::write(&path, "from user").unwrap();
        assert!(ws.apply(&create).is_err());
        let edit = ws
            .prepare_edit("notes.txt", Some("from user"), "edited")
            .unwrap();
        fs::write(&path, "new user edit").unwrap();
        assert!(ws.apply(&edit).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), "new user edit");
    }

    #[test]
    fn rejects_a_replaced_parent_directory() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        let src = root.path().join("src");
        fs::create_dir(&src).unwrap();
        let edit = ws
            .prepare_edit("src/main.rs", None, "fn main() {}")
            .unwrap();
        let directory = ws.prepare_directory("src/generated").unwrap();
        fs::rename(&src, root.path().join("old-src")).unwrap();
        fs::create_dir(&src).unwrap();
        assert!(ws.apply(&edit).is_err());
        assert!(ws.create_directory(&directory).is_err());
        assert_eq!(fs::read_dir(src).unwrap().count(), 0);
    }

    #[test]
    fn protected_directories_cannot_be_selected_as_workspaces() {
        let root = tempfile::tempdir().unwrap();
        for name in [".git", ".SSH", ".aws", ".adaption-code"] {
            let protected = root.path().join(name);
            let nested = protected.join("nested");
            fs::create_dir_all(&nested).unwrap();
            assert!(Workspace::open(Some(protected)).is_err(), "{name}");
            assert!(Workspace::open(Some(nested)).is_err(), "{name}");
        }
        let alias = root.path().join("alias");
        symlink(root.path().join(".git"), &alias).unwrap();
        assert!(Workspace::open(Some(alias)).is_err());
        assert!(Workspace::open(Some(root.path().into())).is_ok());
    }

    #[test]
    fn edit_preserves_permissions_and_refuses_concurrent_metadata_changes() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        let path = root.path().join("script");
        fs::write(&path, "original\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let first = ws
            .prepare_edit("script", Some("original"), "updated")
            .unwrap();
        ws.apply(&first).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o777, 0o755);
        let second = ws.prepare_edit("script", Some("updated"), "new").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(ws.apply(&second).is_err());
        assert_eq!(fs::read_to_string(&path).unwrap(), "updated\n");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[test]
    fn rejects_binary_oversized_and_out_of_range_reads() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        fs::write(root.path().join("binary"), [0, 1]).unwrap();
        fs::write(root.path().join("invalid"), [0xff]).unwrap();
        fs::write(root.path().join("large"), vec![b'x'; MAX_FILE + 1]).unwrap();
        fs::write(root.path().join("text"), "one\ntwo\n").unwrap();
        for path in ["binary", "invalid", "large"] {
            assert!(ws.read(path, 1, 1).is_err(), "{path}");
        }
        for (start, end) in [(0, 1), (2, 1), (1, 201)] {
            assert!(ws.read("text", start, end).is_err());
        }
        assert_eq!(ws.read("text", 2, 2).unwrap(), "2: two\n");
    }

    #[test]
    fn rejects_escapes_links_secrets_and_ambiguous_replacements() {
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        let external = other.path().join("file");
        fs::write(&external, "outside").unwrap();
        symlink(other.path(), root.path().join("outside")).unwrap();
        symlink(&external, root.path().join("link")).unwrap();
        fs::hard_link(&external, root.path().join("hardlink")).unwrap();
        fs::write(root.path().join(".env"), "secret").unwrap();
        fs::write(root.path().join("repeat"), "aaaa").unwrap();
        for path in [
            "../file",
            "/etc/passwd",
            "outside/file",
            "link",
            "hardlink",
            ".env",
        ] {
            assert!(ws.read(path, 1, 3).is_err(), "{path}");
        }
        assert!(ws.prepare_edit("repeat", Some("aaa"), "b").is_err());
    }

    #[test]
    fn search_and_list_honor_ignore_patterns() {
        let root = tempfile::tempdir().unwrap();
        let ws = Workspace::open(Some(root.path().into())).unwrap();
        fs::write(root.path().join(".gitignore"), "ignored.txt\n").unwrap();
        fs::write(root.path().join("ignored.txt"), "needle").unwrap();
        fs::write(root.path().join("source.txt"), "NEEDLE").unwrap();
        let search = ws.search(".", "needle", false).unwrap();
        assert!(search.contains("source.txt:1: NEEDLE"));
        assert!(!search.contains("ignored.txt"));
        let list = ws.list(".", 3).unwrap();
        assert!(list.contains("source.txt"));
        assert!(!list.contains("ignored.txt"));
    }
}
