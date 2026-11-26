//! Scoped folders: named roots on a device's own disk that the user lets threads reach
//! beyond their workspace, each read-only or read-write (Proposal v3, "per-device
//! permissions: scoped folders").
//!
//! - **Configured on the device** (`grouper connect --folder name=path[:ro|rw]`, or
//!   `<config dir>/grouper/folders.json`); the hub only learns names and modes.
//! - **Path syntax:** `@<name>/<path>` in the file tools (`@projects/app/README.md`),
//!   `@<name>` for the folder itself, in `run_shell`'s `cwd`, and in a CLI agent's
//!   `working_directory`. Paths are confined exactly like the workspace: no `..`, no
//!   absolute or drive paths, and whatever already exists must resolve (through
//!   symlinks, junctions and other reparse points) inside the folder; on Windows the
//!   prefix check ignores case.
//! - **Gating** is separate from the workspace ([`gate`]): with the tool on Auto, reads
//!   run and writes ask ("writes to @docs on Laptop"), a shell there asks; Manual asks;
//!   Bypass runs. Writes into a `read` folder are refused by the tool at every level.

use std::path::{Component, Path, PathBuf};

use core_protocol::{FolderInfo, FolderMode, Permission, valid_folder_name};
use serde_json::Value;

use crate::permission::shell_risk;
use crate::tools::Workspace;

/// A folder as configured on the device.
#[derive(Clone, Debug, PartialEq)]
pub struct FolderSpec {
    pub name: String,
    pub path: PathBuf,
    pub mode: FolderMode,
}

impl FolderSpec {
    /// `name=path[:ro|rw]` (also `:read` / `:read_write`). Without a mode the folder is read-only.
    pub fn parse_arg(arg: &str) -> Result<Self, String> {
        let (name, rest) = arg.split_once('=').ok_or_else(|| format!("{arg:?}: expected name=path[:ro|rw]"))?;
        let (path, mode) = match rest.rsplit_once(':') {
            Some((p, m)) if !p.is_empty() => match FolderMode::parse(m) {
                Some(mode) => (p, mode),
                None => (rest, FolderMode::Read),
            },
            _ => (rest, FolderMode::Read),
        };
        Self::new(name, path, mode)
    }

    fn new(name: &str, path: &str, mode: FolderMode) -> Result<Self, String> {
        let name = name.trim();
        if !valid_folder_name(name) {
            return Err(format!("folder name {name:?} must be 1-32 letters, digits, `_` or `-`"));
        }
        let path = path.trim();
        if path.is_empty() {
            return Err(format!("folder {name} has no path"));
        }
        Ok(Self { name: name.into(), path: expand_home(path), mode })
    }
}

/// `~/x` (or `~\x`) under the user's home folder.
fn expand_home(path: &str) -> PathBuf {
    let home = || std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from);
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => home().map(|h| h.join(rest)).unwrap_or_else(|| PathBuf::from(path)),
        None => PathBuf::from(path),
    }
}

/// Read a device's folder config: `{"folders": {"<name>": {"path": "...", "mode": "read" | "read_write"}}}`.
/// A string value is shorthand for `path[:ro|rw]`. `mode` defaults to `read`.
pub fn parse_config(text: &str) -> Result<Vec<FolderSpec>, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let Some(folders) = v.get("folders") else { return Ok(Vec::new()) };
    let folders = folders.as_object().ok_or("`folders` must be an object of name: {path, mode}")?;
    let mut out = Vec::new();
    for (name, entry) in folders {
        if name.contains('=') {
            return Err(format!("folder name {name:?} must not contain `=`"));
        }
        if out.iter().any(|spec: &FolderSpec| spec.name.eq_ignore_ascii_case(name)) {
            return Err(format!("folder name {name:?} differs only by case from another folder"));
        }
        let spec = match entry {
            Value::String(s) => {
                let (path, mode) = match s.rsplit_once(':') {
                    Some((p, m)) if !p.is_empty() => match FolderMode::parse(m) {
                        Some(mode) => (p, mode),
                        None => (s.as_str(), FolderMode::Read),
                    },
                    _ => (s.as_str(), FolderMode::Read),
                };
                FolderSpec::new(name, path, mode)?
            }
            Value::Object(o) => {
                let path = o.get("path").and_then(Value::as_str).ok_or_else(|| format!("folder {name} has no path"))?;
                let mode = match o.get("mode").and_then(Value::as_str) {
                    None => FolderMode::Read,
                    Some(m) => FolderMode::parse(m).ok_or_else(|| format!("folder {name}: mode must be read or read_write"))?,
                };
                FolderSpec::new(name, path, mode)?
            }
            _ => return Err(format!("folder {name}: expected {{\"path\": …, \"mode\": …}}")),
        };
        out.push(spec);
    }
    Ok(out)
}

/// A folder threads can reach: its canonical root and mode.
#[derive(Clone, Debug)]
pub struct ScopedFolder {
    pub name: String,
    pub root: PathBuf,
    pub mode: FolderMode,
}

/// A device's scoped folders, checked once when the device starts.
#[derive(Clone, Debug, Default)]
pub struct Folders {
    list: Vec<ScopedFolder>,
}

impl Folders {
    /// Keep the folders that exist; the strings say what was left out and why.
    pub fn open(specs: &[FolderSpec]) -> (Self, Vec<String>) {
        let mut list: Vec<ScopedFolder> = Vec::new();
        let mut warnings = Vec::new();
        for spec in specs {
            if list.iter().any(|f| f.name.eq_ignore_ascii_case(&spec.name)) {
                warnings.push(format!("folder {} is listed twice; keeping the first", spec.name));
                continue;
            }
            match spec.path.canonicalize() {
                Ok(root) if root.is_dir() => list.push(ScopedFolder { name: spec.name.clone(), root, mode: spec.mode }),
                Ok(_) => warnings.push(format!("folder {}: {} is not a folder", spec.name, spec.path.display())),
                Err(e) => warnings.push(format!("folder {}: {}: {e}", spec.name, spec.path.display())),
            }
        }
        (Self { list }, warnings)
    }

    pub fn get(&self, name: &str) -> Option<&ScopedFolder> {
        self.list.iter().find(|f| f.name.eq_ignore_ascii_case(name))
    }

    pub fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &ScopedFolder> {
        self.list.iter()
    }

    /// What the device reports to the hub: names and modes, never paths.
    pub fn infos(&self) -> Vec<FolderInfo> {
        self.list.iter().map(|f| FolderInfo { name: f.name.clone(), mode: f.mode }).collect()
    }

    fn unknown(&self, name: &str) -> String {
        if self.list.is_empty() {
            format!("@{name}: this device has no scoped folders")
        } else {
            let names: Vec<String> = self.list.iter().map(|f| format!("@{} ({})", f.name, f.mode.as_str())).collect();
            format!("@{name}: no such scoped folder here; there are {}", names.join(", "))
        }
    }
}

/// `@name/rest` (or `@name\rest`, or just `@name`) → `(name, rest)`.
pub fn split_scoped(path: &str) -> Option<(&str, &str)> {
    let p = path.trim().strip_prefix('@')?;
    Some(match p.find(['/', '\\']) {
        Some(i) => (&p[..i], &p[i + 1..]),
        None => (p, ""),
    })
}

/// Whether `path` is inside `root` (both canonical). Windows paths compare without case.
pub fn within(path: &Path, root: &Path) -> bool {
    #[cfg(windows)]
    {
        let parts = |p: &Path| p.components().map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>();
        let (p, r) = (parts(path), parts(root));
        p.len() >= r.len() && p[..r.len()] == r[..]
    }
    #[cfg(not(windows))]
    {
        path.starts_with(root)
    }
}

/// `rel` under the canonical `root`, refusing absolute and drive paths, `..`, and
/// anything whose existing part resolves outside `root` (symlinks, junctions and other
/// reparse points are followed to check; a dangling link is refused, not skipped).
/// `what` names the root in errors.
pub fn confine(root: &Path, rel: &str, what: &str) -> Result<PathBuf, String> {
    let rel = rel.trim();
    let p = Path::new(if rel.is_empty() { "." } else { rel });
    if p.is_absolute()
        || p.components().any(|c| matches!(c, Component::ParentDir | Component::Prefix(_) | Component::RootDir))
    {
        return Err(format!("{rel:?} is outside {what}; use a relative path without `..`"));
    }
    #[cfg(windows)]
    for component in p.components() {
        if let Component::Normal(name) = component {
            let name = name.to_string_lossy();
            if name.contains(':') {
                return Err(format!("{rel:?} is unsafe on Windows; path components must not contain `:`"));
            }
            if name.ends_with(['.', ' ']) {
                return Err(format!("{rel:?} is unsafe on Windows; path components must not end in a dot or space"));
            }
        }
    }
    let full = root.join(p);
    let mut existing = full.clone();
    while std::fs::symlink_metadata(&existing).is_err() {
        if existing == root || !existing.pop() {
            return Err(format!("{what} is missing"));
        }
    }
    let real = existing.canonicalize().map_err(|e| format!("{rel}: {e}"))?;
    if !within(&real, root) {
        return Err(format!("{rel:?} leads outside {what}"));
    }
    Ok(full)
}

/// A path the way a person or another program should see it (no `\\?\` prefix).
pub fn display_path(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Where a tool's path landed.
pub struct Located<'a> {
    /// `None` for the thread's own workspace.
    pub folder: Option<&'a ScopedFolder>,
    pub full: PathBuf,
    /// The workspace root or the folder's root.
    pub root: &'a Path,
}

impl Workspace {
    /// A workspace-relative path, or `@<folder>/<path>` in one of the device's scoped folders.
    pub fn locate(&self, path: &str) -> Result<Located<'_>, String> {
        match split_scoped(path) {
            None => Ok(Located { folder: None, full: self.resolve(path)?, root: self.root() }),
            Some((name, rest)) => {
                let folder = self.folders().get(name).ok_or_else(|| self.folders().unknown(name))?;
                let full = confine(&folder.root, rest, &format!("@{}", folder.name))?;
                Ok(Located { folder: Some(folder), full, root: &folder.root })
            }
        }
    }
}

/// A file or shell call that reaches a scoped folder.
pub struct FolderUse<'a> {
    pub folder: &'a ScopedFolder,
    pub write: bool,
    pub shell: bool,
}

/// Which scoped folder (if any, and known) a thread tool call reaches.
pub fn folder_use<'a>(tool: &str, input: &Value, workspace: &'a Workspace) -> Option<FolderUse<'a>> {
    let (arg, write, shell) = match tool {
        "read_file" | "list_files" => ("path", false, false),
        "write_file" => ("path", true, false),
        "run_shell" => ("cwd", false, true),
        _ => return None,
    };
    let (name, _) = split_scoped(input.get(arg)?.as_str()?)?;
    let folder = workspace.folders().get(name)?;
    Some(FolderUse { folder, write, shell })
}

/// The approval decision for a call into a scoped folder, gated apart from the
/// workspace: `None` when the call doesn't reach one (the usual rules apply), else
/// `Some(reason to ask, or None to run)`. On Auto reads run, writes ask and a shell
/// there asks; Manual asks; Bypass runs. A write into a `read` folder isn't asked
/// about: the tool refuses it.
pub fn gate(level: Permission, tool: &str, input: &Value, workspace: &Workspace) -> Option<Option<String>> {
    let u = folder_use(tool, input, workspace)?;
    if u.write && u.folder.mode == FolderMode::Read {
        return Some(None);
    }
    let place = format!("@{} on {}", u.folder.name, workspace.device_label());
    Some(match level {
        Permission::Bypass => None,
        Permission::Manual => Some(format!("{tool} is set to Manual")),
        Permission::Auto if u.shell => {
            let cmd = input.get("command").and_then(Value::as_str).unwrap_or_default();
            Some(match shell_risk(cmd) {
                Some(r) => format!("run_shell runs in {place}, and the command {r}"),
                None => format!("run_shell runs in {place}"),
            })
        }
        Permission::Auto if u.write => Some(format!("writes to {place}")),
        Permission::Auto => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::check_call;
    use crate::tools::{file_tools, shell_tool};
    use serde_json::json;
    use std::sync::Arc;

    struct Fixture {
        base: PathBuf,
        ws: Arc<Workspace>,
        notes: PathBuf,
        out: PathBuf,
        outside: PathBuf,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = remove_link(&self.notes.join("escape"));
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn remove_link(link: &Path) -> std::io::Result<()> {
        if cfg!(windows) { std::fs::remove_dir(link) } else { std::fs::remove_file(link) }
    }

    /// `notes` (read) and `out` (read_write) next to an `outside` folder with a secret.
    fn fixture(tag: &str) -> Fixture {
        let base = std::env::temp_dir().join(format!("grouper-folders-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (notes, out, outside) = (base.join("Notes"), base.join("out"), base.join("outside"));
        for d in [&notes, &out, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(notes.join("todo.txt"), "buy milk").unwrap();
        std::fs::create_dir_all(notes.join("sub")).unwrap();
        std::fs::write(notes.join("sub").join("deep.txt"), "deep").unwrap();
        std::fs::write(outside.join("secret.txt"), "top-secret").unwrap();
        let specs = vec![
            FolderSpec { name: "notes".into(), path: notes.clone(), mode: FolderMode::Read },
            FolderSpec { name: "Out".into(), path: out.clone(), mode: FolderMode::ReadWrite },
            FolderSpec { name: "gone".into(), path: base.join("missing"), mode: FolderMode::Read },
        ];
        let (folders, warnings) = Folders::open(&specs);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("gone"));
        let ws = Workspace::create_scoped(base.join("ws"), Arc::new(folders), "Laptop").unwrap();
        Fixture { base, ws, notes, out, outside }
    }

    fn link_dir(link: &Path, target: &Path) -> bool {
        #[cfg(unix)]
        return std::os::unix::fs::symlink(target, link).is_ok();
        #[cfg(windows)]
        return std::process::Command::new("cmd")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .output()
            .is_ok_and(|o| o.status.success());
    }

    #[test]
    fn specs_and_config() {
        let s = FolderSpec::parse_arg("projects=C:\\Users\\me\\GitHub:rw").unwrap();
        assert_eq!((s.name.as_str(), s.path.clone(), s.mode), ("projects", PathBuf::from("C:\\Users\\me\\GitHub"), FolderMode::ReadWrite));
        let s = FolderSpec::parse_arg("docs=C:\\Docs").unwrap();
        assert_eq!((s.path.clone(), s.mode), (PathBuf::from("C:\\Docs"), FolderMode::Read));
        let s = FolderSpec::parse_arg("x=/srv/data:ro").unwrap();
        assert_eq!((s.path.clone(), s.mode), (PathBuf::from("/srv/data"), FolderMode::Read));
        assert!(FolderSpec::parse_arg("no-equals").is_err());
        assert!(FolderSpec::parse_arg("bad name=/x").is_err());
        assert!(FolderSpec::parse_arg("x=").is_err());

        let specs = parse_config(
            r#"{"folders": {"a": {"path": "/a", "mode": "read_write"}, "b": {"path": "/b"}, "c": "/c:rw"}}"#,
        )
        .unwrap();
        let got: Vec<(String, FolderMode)> = specs.into_iter().map(|s| (s.name, s.mode)).collect();
        assert_eq!(
            got,
            vec![("a".into(), FolderMode::ReadWrite), ("b".into(), FolderMode::Read), ("c".into(), FolderMode::ReadWrite)]
        );
        assert!(parse_config(r#"{"folders": {"a": {"path": "/a", "mode": "everything"}}}"#).is_err());
        assert!(parse_config(r#"{"folders": {"a=b": "/tmp"}}"#).is_err());
        assert!(parse_config(r#"{"folders": {"Docs": "/a", "docs": "/b"}}"#).is_err());
        assert!(parse_config("{}").unwrap().is_empty());
        assert_eq!(split_scoped("@a/b/c"), Some(("a", "b/c")));
        assert_eq!(split_scoped(r"@a\b"), Some(("a", "b")));
        assert_eq!(split_scoped("@a"), Some(("a", "")));
        assert_eq!(split_scoped("a/@b"), None);
    }

    #[tokio::test]
    async fn confinement_in_scoped_folders() {
        let f = fixture("confine");
        let tools = file_tools(&f.ws);
        let (read, write, list) = (&tools[0], &tools[1], &tools[2]);

        let r = read.call(json!({"path": "@notes/todo.txt"})).await;
        assert_eq!((r.is_error, r.content.as_str()), (false, "buy milk"));
        // Folder names ignore case; so does the path on Windows.
        assert_eq!(read.call(json!({"path": "@NOTES/sub/deep.txt"})).await.content, "deep");
        #[cfg(windows)]
        assert_eq!(read.call(json!({"path": "@notes/SUB/Deep.TXT"})).await.content, "deep");
        for bad in ["@notes/../outside/secret.txt", "@notes/sub/../../outside/secret.txt", "@notes//etc/passwd"] {
            let r = read.call(json!({"path": bad})).await;
            assert!(r.is_error, "{bad}: {r:?}");
        }
        #[cfg(windows)]
        for bad in [r"@notes\..\outside\secret.txt", r"@notes/C:\Windows\win.ini", r"@notes/\Windows\win.ini"] {
            assert!(read.call(json!({"path": bad})).await.is_error, "{bad}");
        }
        let r = read.call(json!({"path": "@nope/x"})).await;
        assert!(r.is_error && r.content.contains("@notes (read)"), "{r:?}");
        assert!(read.call(json!({"path": "@gone/x"})).await.is_error);

        // Writes into a read folder are refused; a read_write folder takes them.
        let w = write.call(json!({"path": "@notes/new.txt", "content": "x"})).await;
        assert!(w.is_error && w.content.contains("read-only"), "{w:?}");
        assert!(!f.notes.join("new.txt").exists());
        let w = write.call(json!({"path": "@out/report/r.md", "content": "done"})).await;
        assert!(!w.is_error, "{w:?}");
        assert_eq!(std::fs::read_to_string(f.out.join("report").join("r.md")).unwrap(), "done");
        assert!(write.call(json!({"path": "@out/../outside/x.txt", "content": "x"})).await.is_error);

        // A junction (Windows) or symlink out of the folder: listed as a link, never followed.
        if link_dir(&f.notes.join("escape"), &f.outside) {
            assert!(read.call(json!({"path": "@notes/escape/secret.txt"})).await.is_error);
            assert!(list.call(json!({"path": "@notes/escape"})).await.is_error);
            if link_dir(&f.out.join("escape"), &f.outside) {
                let w = write.call(json!({"path": "@out/escape/planted.txt", "content": "x"})).await;
                assert!(w.is_error, "{w:?}");
                assert!(!f.outside.join("planted.txt").exists());
                let _ = remove_link(&f.out.join("escape"));
            }
            let listing = list.call(json!({"path": "@notes"})).await;
            assert!(!listing.is_error, "{listing:?}");
            let lines: Vec<&str> = listing.content.lines().collect();
            assert!(lines.contains(&"@notes/todo.txt"), "{lines:?}");
            assert!(lines.contains(&"@notes/sub/deep.txt"), "{lines:?}");
            assert!(lines.contains(&"@notes/escape@"), "{lines:?}");
            assert!(!lines.iter().any(|l| l.contains("secret")), "{lines:?}");
        }
        // A dangling link can't be used to write outside either.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(f.outside.join("new.txt"), f.out.join("dangling")).unwrap();
            assert!(write.call(json!({"path": "@out/dangling", "content": "x"})).await.is_error);
            assert!(!f.outside.join("new.txt").exists());
        }
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_alias_components_are_refused_before_junction_walk() {
        let f = fixture("windows-aliases");
        let link = f.notes.join("escape");
        if !link_dir(&link, &f.outside) {
            return;
        }
        let tools = file_tools(&f.ws);
        for path in ["@notes/escape:evil/secret.txt", "@notes/escape./secret.txt", "@notes/escape /secret.txt"] {
            let result = tools[0].call(json!({"path": path})).await;
            assert!(result.is_error, "{path}: {result:?}");
            assert!(!result.content.contains("top-secret"), "{path}: {result:?}");
        }
        for path in ["escape:evil/secret.txt", "escape./secret.txt", "escape /secret.txt"] {
            assert!(confine(&f.notes, path, "the workspace").is_err(), "{path}");
        }
    }

    #[test]
    fn gating_per_level() {
        let f = fixture("gate");
        let ws = &f.ws;
        let check = |level, tool: &str, input: Value| {
            let mut input = input;
            check_call(level, tool, &mut input, ws)
        };
        // The workspace stays as it was: file tools on Auto run.
        assert_eq!(check(Permission::Auto, "write_file", json!({"path": "a.txt", "content": "x"})), None);
        // Scoped folder on Auto: reads run, writes ask with the place.
        assert_eq!(check(Permission::Auto, "read_file", json!({"path": "@notes/todo.txt"})), None);
        assert_eq!(check(Permission::Auto, "list_files", json!({"path": "@out"})), None);
        let why = check(Permission::Auto, "write_file", json!({"path": "@out/x.txt", "content": "x"})).unwrap();
        assert_eq!(why, "writes to @Out on Laptop");
        // Writes into a read folder go to the tool, which refuses them.
        assert_eq!(check(Permission::Auto, "write_file", json!({"path": "@notes/x.txt", "content": "x"})), None);
        // Manual asks, Bypass runs.
        assert!(check(Permission::Manual, "read_file", json!({"path": "@notes/todo.txt"})).unwrap().contains("Manual"));
        assert_eq!(check(Permission::Bypass, "write_file", json!({"path": "@out/x.txt", "content": "x"})), None);
        // A shell in a scoped folder asks on Auto, even for a harmless command.
        let why = check(Permission::Auto, "run_shell", json!({"command": "git status", "cwd": "@out"})).unwrap();
        assert!(why.contains("@Out on Laptop"), "{why}");
        let why = check(Permission::Auto, "run_shell", json!({"command": "rm -rf x", "cwd": "@notes/sub"})).unwrap();
        assert!(why.contains("@notes") && why.contains("rm"), "{why}");
        assert_eq!(check(Permission::Bypass, "run_shell", json!({"command": "git status", "cwd": "@out"})), None);
        assert_eq!(check(Permission::Auto, "run_shell", json!({"command": "git status"})), None);
        assert_eq!(check(Permission::Auto, "run_shell", json!({"command": "git status", "cwd": "sub"})), None);
    }

    #[tokio::test]
    async fn shell_cwd_in_a_scoped_folder() {
        let f = fixture("shell");
        let sh = shell_tool(&f.ws);
        let cmd = if cfg!(windows) { "Get-Content todo.txt" } else { "cat todo.txt" };
        let r = sh.call(json!({"command": cmd, "cwd": "@notes"})).await;
        assert!(!r.is_error && r.content.contains("buy milk"), "{r:?}");
        let r = sh.call(json!({"command": cmd, "cwd": "@notes/../outside"})).await;
        assert!(r.is_error && !r.content.contains("top-secret"), "{r:?}");
        let r = sh.call(json!({"command": cmd, "cwd": "@notes/todo.txt"})).await;
        assert!(r.is_error, "{r:?}");
        assert!(sh.def().description.contains("@"));
    }
}
