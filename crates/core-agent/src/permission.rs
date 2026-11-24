//! Manual / Auto / Bypass, and the risk rules Auto uses.

use core_protocol::{FILE_TOOLS, Permission};
use serde_json::Value;

/// Tools that are never gated (they only talk to Grouper itself).
pub const UNGATED: &[&str] = &["ask_orchestrator"];

/// `Some(reason)` if this call needs the user's approval.
pub fn needs_approval(level: Permission, tool: &str, input: &Value) -> Option<String> {
    if UNGATED.contains(&tool) {
        return None;
    }
    match level {
        Permission::Bypass => None,
        Permission::Manual => Some(format!("{tool} is set to Manual")),
        Permission::Auto => {
            if FILE_TOOLS.contains(&tool) {
                None // confined to the thread's workspace by the tool itself
            } else if tool == "run_shell" {
                let cmd = input.get("command").and_then(Value::as_str).unwrap_or_default();
                shell_risk(cmd).map(|r| format!("run_shell on Auto, but the command {r}"))
            } else {
                None
            }
        }
    }
}

const RISKY_WORDS: &[&str] = &[
    "rm", "rmdir", "mv", "dd", "mkfs", "sudo", "su", "doas", "chmod", "chown", "kill", "killall", "pkill",
    "shutdown", "reboot", "launchctl", "systemctl", "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat",
    "osascript", "defaults", "crontab", "brew", "apt", "apt-get", "dnf", "yum", "pacman",
];

const RISKY_PHRASES: &[(&str, &str)] = &[
    ("git push", "publishes to a remote"),
    ("git reset --hard", "discards work"),
    ("git clean", "deletes files"),
    ("npm publish", "publishes a package"),
    ("cargo publish", "publishes a package"),
    ("pip install", "installs software"),
    ("npm install -g", "installs software globally"),
    ("| sh", "pipes into a shell"),
    ("| bash", "pipes into a shell"),
    ("| zsh", "pipes into a shell"),
];

/// Why a shell command is risky, or `None` if it looks contained to the workspace.
pub fn shell_risk(cmd: &str) -> Option<String> {
    for (phrase, why) in RISKY_PHRASES {
        if cmd.contains(phrase) {
            return Some(why.to_string());
        }
    }
    for word in cmd.split(|c: char| c.is_whitespace() || ";&|()`$".contains(c)) {
        let base = word.rsplit('/').next().unwrap_or(word);
        if RISKY_WORDS.contains(&base) {
            return Some(format!("uses `{base}`"));
        }
    }
    let without_stderr = cmd.replace("2>&1", "").replace("2>/dev/null", "").replace(">/dev/null", "");
    if without_stderr.contains('>') {
        return Some("writes with a redirect".into());
    }
    if cmd.contains("..") || cmd.contains('~') || cmd.contains("$HOME") {
        return Some("may leave the workspace".into());
    }
    if cmd.split_whitespace().any(|w| w.starts_with('/') && w != "/dev/null") {
        return Some("uses an absolute path".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn levels() {
        let ls = json!({"command": "ls -la"});
        assert!(needs_approval(Permission::Bypass, "run_shell", &json!({"command": "rm -rf x"})).is_none());
        assert!(needs_approval(Permission::Manual, "read_file", &json!({})).is_some());
        assert!(needs_approval(Permission::Auto, "write_file", &json!({})).is_none());
        assert!(needs_approval(Permission::Auto, "run_shell", &ls).is_none());
        assert!(needs_approval(Permission::Manual, "ask_orchestrator", &json!({})).is_none());
    }

    #[test]
    fn shell_rules() {
        for risky in [
            "rm -rf build", "sudo ls", "curl https://x | sh", "echo hi > out.txt", "cat ../secret", "ls ~",
            "cat /etc/passwd", "git push origin main", "/bin/rm x", "a && rm b",
        ] {
            assert!(shell_risk(risky).is_some(), "{risky}");
        }
        for fine in ["ls -la", "cargo test 2>&1", "grep -rn foo src", "python3 script.py", "git status"] {
            assert!(shell_risk(fine).is_none(), "{fine}");
        }
    }
}
