//! Where a device's scoped folders come from: `<OS config dir>/grouper/folders.json`
//! (override: `GROUPER_FOLDERS_CONFIG`), next to `mcp.json`:
//!
//! ```json
//! { "folders": {
//!     "projects": { "path": "C:\\Users\\me\\Documents\\GitHub", "mode": "read_write" },
//!     "notes": { "path": "~/Notes" },
//!     "media": "D:\\Media:ro" } }
//! ```
//!
//! `mode` is `read` (the default) or `read_write`; a string is `path[:ro|rw]`.
//! `grouper connect --folder name=path[:ro|rw]` adds to (or replaces by name) the file's folders.

use std::path::{Path, PathBuf};

use core_agent::folders::{FolderSpec, parse_config};

pub const CONFIG_ENV: &str = "GROUPER_FOLDERS_CONFIG";

/// The folders file to read: `explicit`, else `GROUPER_FOLDERS_CONFIG`, else
/// `<OS config dir>/grouper/folders.json`.
pub fn config_path(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os(CONFIG_ENV).filter(|p| !p.is_empty()) {
        return Some(PathBuf::from(p));
    }
    dirs::config_dir().map(|d| d.join("grouper").join("folders.json"))
}

/// The file's folders (none when it doesn't exist), then `args` (`name=path[:ro|rw]`),
/// which replace file entries of the same name.
pub fn load(explicit: Option<&Path>, args: &[String]) -> Result<Vec<FolderSpec>, String> {
    let mut specs = match config_path(explicit) {
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(text) => parse_config(&text).map_err(|e| format!("{}: {e}", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => Vec::new(),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        },
        None => Vec::new(),
    };
    for arg in args {
        let spec = FolderSpec::parse_arg(arg)?;
        specs.retain(|s| !s.name.eq_ignore_ascii_case(&spec.name));
        specs.push(spec);
    }
    Ok(specs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_then_flags() {
        let dir = std::env::temp_dir().join(format!("grouper-folders-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("folders.json");
        std::fs::write(&file, r#"{"folders": {"a": {"path": "/a", "mode": "read_write"}, "b": "/b"}}"#).unwrap();
        let specs = load(Some(&file), &["A=/other:ro".into(), "c=/c:rw".into()]).unwrap();
        let got: Vec<(String, String, &str)> =
            specs.iter().map(|s| (s.name.clone(), s.path.to_string_lossy().to_string(), s.mode.as_str())).collect();
        assert_eq!(
            got,
            vec![
                ("b".into(), "/b".into(), "read"),
                ("A".into(), "/other".into(), "read"),
                ("c".into(), "/c".into(), "read_write"),
            ]
        );
        assert!(load(Some(&dir.join("missing.json")), &[]).is_err());
        assert!(load(Some(&file), &["bad".into()]).is_err());
        std::fs::write(&file, r#"{"folders": {"bad=name": "/a"}}"#).unwrap();
        assert!(load(Some(&file), &[]).is_err());
        std::fs::write(&file, r#"{"folders": {"Docs": "/a", "docs": "/b"}}"#).unwrap();
        assert!(load(Some(&file), &[]).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
