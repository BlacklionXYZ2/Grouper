//! A thread's workspace seen from outside the thread: the hub fetching a thread's
//! output files (on the device that ran it, or in its own workspaces). Same
//! confinement as the thread's file tools; links are never followed when listing.

use std::path::Path;

use core_protocol::WorkspaceFile;

use crate::folders::confine;
use crate::tools::{Entry, walk};

/// Most files a listing returns.
pub const MAX_LISTED_FILES: usize = 5000;

/// Why a workspace file couldn't be had.
#[derive(Debug, PartialEq)]
pub enum FileError {
    /// No such workspace or file.
    NotFound(String),
    /// Outside the workspace, a folder, or a link out of it.
    Refused(String),
    TooLarge(String),
}

impl FileError {
    pub fn message(&self) -> &str {
        match self {
            Self::NotFound(m) | Self::Refused(m) | Self::TooLarge(m) => m,
        }
    }
    /// The HTTP status the hub answers with.
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound(_) => 404,
            Self::Refused(_) => 400,
            Self::TooLarge(_) => 413,
        }
    }
    pub fn from_status(status: u16, message: String) -> Self {
        match status {
            404 => Self::NotFound(message),
            413 => Self::TooLarge(message),
            _ => Self::Refused(message),
        }
    }
}

fn open_root(root: &Path) -> Result<std::path::PathBuf, FileError> {
    match root.canonicalize() {
        Ok(r) if r.is_dir() => Ok(r),
        _ => Err(FileError::NotFound("no workspace for this thread here".into())),
    }
}

/// Every file in the workspace at `root` (links are skipped, not followed), sorted;
/// the bool is true when there were more than [`MAX_LISTED_FILES`].
pub fn list_workspace(root: &Path) -> Result<(Vec<WorkspaceFile>, bool), FileError> {
    let root = open_root(root)?;
    let (entries, truncated) = walk(root.clone(), &root, MAX_LISTED_FILES * 4);
    let mut files: Vec<WorkspaceFile> = entries
        .into_iter()
        .filter_map(|(path, e)| match e {
            Entry::File(size) => Some(WorkspaceFile { path, size }),
            _ => None,
        })
        .collect();
    let more = truncated || files.len() > MAX_LISTED_FILES;
    files.truncate(MAX_LISTED_FILES);
    Ok((files, more))
}

/// The bytes of one workspace file, if it is at most `cap` bytes.
pub fn read_workspace_file(root: &Path, path: &str, cap: u64) -> Result<Vec<u8>, FileError> {
    let root = open_root(root)?;
    if path.trim().is_empty() {
        return Err(FileError::Refused("name a file".into()));
    }
    let full = confine(&root, path, "the workspace").map_err(FileError::Refused)?;
    let meta = std::fs::metadata(&full).map_err(|_| FileError::NotFound(format!("{path}: no such file")))?;
    if !meta.is_file() {
        return Err(FileError::Refused(format!("{path} is not a file")));
    }
    if meta.len() > cap {
        return Err(FileError::TooLarge(format!("{path} is {} bytes; the limit is {cap}", meta.len())));
    }
    std::fs::read(&full).map_err(|e| FileError::NotFound(format!("{path}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_and_reads_inside_only() {
        let base = std::env::temp_dir().join(format!("grouper-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join("out")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        std::fs::write(ws.join("out").join("report.md"), "# done").unwrap();
        std::fs::write(ws.join("big.bin"), vec![0u8; 100]).unwrap();
        std::fs::write(base.join("outside").join("secret.txt"), "s").unwrap();

        let (files, more) = list_workspace(&ws).unwrap();
        assert!(!more);
        let got: Vec<(&str, u64)> = files.iter().map(|f| (f.path.as_str(), f.size)).collect();
        assert_eq!(got, vec![("big.bin", 100), ("out/report.md", 6)]);
        assert_eq!(read_workspace_file(&ws, "out/report.md", 1024).unwrap(), b"# done");
        assert_eq!(read_workspace_file(&ws, r"out\report.md", 1024).is_ok(), cfg!(windows));
        assert!(matches!(read_workspace_file(&ws, "big.bin", 10), Err(FileError::TooLarge(_))));
        assert!(matches!(read_workspace_file(&ws, "../outside/secret.txt", 1024), Err(FileError::Refused(_))));
        assert!(matches!(read_workspace_file(&ws, "out", 1024), Err(FileError::Refused(_))));
        assert!(matches!(read_workspace_file(&ws, "nope.txt", 1024), Err(FileError::NotFound(_))));
        assert!(matches!(list_workspace(&base.join("missing")), Err(FileError::NotFound(_))));
        std::fs::remove_dir_all(base).ok();
    }
}
