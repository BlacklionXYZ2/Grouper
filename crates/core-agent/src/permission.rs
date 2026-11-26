//! Manual / Auto / Bypass, and the risk rules Auto uses.

use core_protocol::{FILE_TOOLS, Permission};
use serde_json::Value;

use crate::agents;
use crate::tools::Workspace;

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
            } else if agents::agent_tool(tool).is_some() {
                agents::auto_risk(tool, input, None, &agents::Clamped::default())
            } else {
                None
            }
        }
    }
}

/// The gate for one call in a thread: set a CLI agent's safety arguments for `level`
/// (see [`crate::agents`]), then decide. `Some(reason)` if the call needs the user's
/// approval; the reason mentions what Grouper changed.
pub fn check_call(level: Permission, tool: &str, input: &mut Value, workspace: &Workspace) -> Option<String> {
    // File and shell calls into the device's scoped folders are gated apart from the workspace.
    if let Some(decision) = crate::folders::gate(level, tool, input, workspace) {
        return decision;
    }
    let clamped = agents::clamp(level, tool, input, workspace);
    let unconfined_read_folder = level == Permission::Bypass
        && clamped.read_only_folder
        && agents::agent_tool(tool).is_some_and(|(server, own)| {
            matches!((server, own), ("hermes", "hermes_chat") | ("grok", "grok_chat") | ("opencode", "opencode_chat"))
        });
    let reason = if unconfined_read_folder {
        Some("this agent can't be held read-only in the selected folder".into())
    } else if level == Permission::Auto && agents::agent_tool(tool).is_some() {
        agents::auto_risk(tool, input, Some(workspace), &clamped)
    } else {
        needs_approval(level, tool, input)
    }?;
    Some(if clamped.changes.is_empty() {
        reason
    } else {
        format!("{reason} (Grouper set {})", clamped.changes.join(", "))
    })
}

/// Whole command tokens, compared case-insensitively. Unix and Windows names share one
/// list: a command string is judged the same wherever the thread happens to run.
const RISKY_WORDS: &[&str] = &[
    // Unix
    "rm", "rmdir", "mv", "dd", "mkfs", "sudo", "su", "doas", "chmod", "chown", "kill", "killall", "pkill",
    "shutdown", "reboot", "launchctl", "systemctl", "curl", "wget", "ssh", "scp", "sftp", "rsync", "nc", "ncat",
    "osascript", "defaults", "crontab", "brew", "apt", "apt-get", "dnf", "yum", "pacman",
    // Windows delete / move
    "remove-item", "ri", "del", "erase", "rd", "clear-content", "clear-recyclebin",
    "move-item", "mi", "move", "ren", "rename-item",
    // Windows network
    "invoke-webrequest", "iwr", "invoke-restmethod", "irm", "start-bitstransfer", "test-netconnection", "ftp",
    // Windows arbitrary execution
    "invoke-expression", "iex", "start-process", "saps", "start", "powershell", "pwsh", "cmd", "add-type",
    // Windows system changes
    "set-executionpolicy", "reg", "regedit", "set-itemproperty", "new-itemproperty", "remove-itemproperty",
    "sc", "new-service", "set-service", "net", "netsh", "schtasks", "register-scheduledtask",
    "bcdedit", "diskpart", "format", "icacls", "takeown", "cacls", "stop-process", "spps", "taskkill",
    "stop-computer", "restart-computer", "wsl",
    // Windows installs and writes
    "winget", "choco", "scoop", "install-module", "install-package", "msiexec",
    "out-file", "set-content", "add-content", "new-item",
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
///
/// Unix and Windows (PowerShell and cmd) rules both apply to the command string.
/// Names match whole tokens, so `format-table` is not `format`.
pub fn shell_risk(cmd: &str) -> Option<String> {
    let lower = cmd.to_ascii_lowercase();
    for (phrase, why) in RISKY_PHRASES {
        if lower.contains(phrase) {
            return Some(why.to_string());
        }
    }
    for word in command_tokens(cmd) {
        let base = command_basename(word);
        if token_is_risky(base) {
            return Some(format!("uses `{base}`"));
        }
    }
    if lower.contains("net.webclient") {
        return Some("uses `Net.WebClient`".into());
    }
    if lower.contains("hklm:") || lower.contains("hkcu:") {
        return Some("touches the registry".into());
    }
    if call_operator_runs_path(cmd) {
        return Some("runs a path with `&`".into());
    }
    let without_stderr = cmd.replace("2>&1", "").replace("2>/dev/null", "").replace(">/dev/null", "");
    if without_stderr.contains('>') {
        return Some("writes with a redirect".into());
    }
    if leaves_workspace(cmd) {
        return Some("may leave the workspace".into());
    }
    if cmd.split_whitespace().any(|w| w.starts_with('/') && w != "/dev/null") {
        return Some("uses an absolute path".into());
    }
    None
}

fn command_tokens(cmd: &str) -> impl Iterator<Item = &str> {
    cmd.split(|c: char| c.is_whitespace() || ";&|(){}`$<>".contains(c)).filter(|word| !word.is_empty())
}

fn command_basename(word: &str) -> &str {
    let word = strip_wrapping_quotes(word);
    word.rsplit(['/', '\\']).next().unwrap_or(word)
}

fn strip_wrapping_quotes(word: &str) -> &str {
    let bytes = word.as_bytes();
    if bytes.len() >= 2 {
        let (open, close) = (bytes[0], bytes[bytes.len() - 1]);
        if (open == b'"' && close == b'"') || (open == b'\'' && close == b'\'') {
            return &word[1..bytes.len() - 1];
        }
    }
    word
}

fn token_is_risky(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if listed(&lower) {
        return true;
    }
    // `sc.exe`, `cmd.exe`, `format.com`: the bare name is what the rules name.
    match lower.rsplit_once('.') {
        Some((stem, "exe" | "com" | "bat" | "cmd")) => listed(stem),
        _ => false,
    }
}

fn listed(word: &str) -> bool {
    RISKY_WORDS.contains(&word)
}

fn leaves_workspace(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    cmd.contains("..")
        || cmd.contains('~')
        || cmd.contains("$HOME")
        || contains_ident(&lower, "$home")
        || lower.contains("env:userprofile")
        || lower.contains("env:appdata")
        || lower.contains("%userprofile%")
        || lower.contains("%appdata%")
        || has_drive_path(cmd)
        || has_unc_path(cmd)
}

/// `$HOME` / `$home` as its own variable. `$homepage` does not count.
fn contains_ident(hay: &str, needle: &str) -> bool {
    hay.match_indices(needle).any(|(i, _)| {
        let end = i + needle.len();
        end == hay.len() || {
            let b = hay.as_bytes()[end];
            !b.is_ascii_alphanumeric() && b != b'_'
        }
    })
}

/// `C:\` and `D:/`. The letter must start a token, so `https://` is not a drive.
fn has_drive_path(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    if b.len() < 3 {
        return false;
    }
    (0..b.len() - 2).any(|i| {
        b[i].is_ascii_alphabetic()
            && b[i + 1] == b':'
            && (b[i + 2] == b'\\' || b[i + 2] == b'/')
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
    })
}

/// UNC path `\\server`. A trailing `\\` with nothing after it is not one.
fn has_unc_path(cmd: &str) -> bool {
    let b = cmd.as_bytes();
    b.windows(4).any(|w| {
        w[0] == b'\\' && w[1] == b'\\' && w[2].is_ascii_alphanumeric() && w[3].is_ascii_alphanumeric()
    })
}

/// PowerShell call operator (`& .\script.ps1`), not `&&`, `2>&1`, or `>&`.
fn call_operator_runs_path(cmd: &str) -> bool {
    for (i, _) in cmd.match_indices('&') {
        let prev = if i == 0 { None } else { cmd[..i].chars().next_back() };
        let rest = &cmd[i + '&'.len_utf8()..];
        let next = rest.chars().next();
        if prev == Some('>') || next == Some('&') || next == Some('>') {
            continue;
        }
        if starts_with_path(rest.trim_start()) {
            return true;
        }
    }
    false
}

fn starts_with_path(rest: &str) -> bool {
    let rest = rest.trim_start_matches(['"', '\'']);
    if rest.starts_with("./")
        || rest.starts_with(".\\")
        || rest.starts_with("..")
        || rest.starts_with('~')
        || rest.starts_with('\\')
        || rest.starts_with('/')
    {
        return true;
    }
    let b = rest.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')
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
    fn agent_calls_through_the_gate() {
        let dir = std::env::temp_dir().join(format!("grouper-gate-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();
        let root = agents::display_root(&ws);
        let t = "codex__codex_chat";

        // Auto: clamped, runs without asking.
        let mut input = json!({"message": "Write tests"});
        assert_eq!(check_call(Permission::Auto, t, &mut input, &ws), None);
        assert_eq!((&input["sandbox"], &input["working_directory"]), (&json!("workspace-write"), &json!(root)));
        // Auto: a bypass request is clamped and asks, saying what was set.
        let mut input = json!({"message": "Write tests", "sandbox": "danger-full-access"});
        let reason = check_call(Permission::Auto, t, &mut input, &ws).unwrap();
        assert!(reason.contains("danger-full-access") && reason.contains("workspace-write"), "{reason}");
        assert_eq!(input["sandbox"], "workspace-write");
        // Auto: outside the workspace asks; Bypass allows it.
        let away = std::env::temp_dir().to_string_lossy().to_string();
        let mut input = json!({"message": "hi", "working_directory": away});
        assert!(check_call(Permission::Auto, t, &mut input, &ws).unwrap().contains("outside"));
        let mut input = json!({"message": "hi", "working_directory": away});
        assert_eq!(check_call(Permission::Bypass, t, &mut input, &ws), None);
        assert_eq!(input["sandbox"], "danger-full-access");
        // Manual always asks, with the clamped arguments.
        let mut input = json!({"message": "hi"});
        assert!(check_call(Permission::Manual, t, &mut input, &ws).unwrap().contains("Manual"));
        assert_eq!(input["sandbox"], "workspace-write");
        // list tools never ask on Auto.
        let mut input = json!({});
        assert_eq!(check_call(Permission::Auto, "catalog__list_available_agents", &mut input, &ws), None);
        // Other tools behave as before.
        let mut input = json!({"command": "rm -rf x"});
        assert!(check_call(Permission::Auto, "run_shell", &mut input, &ws).is_some());
        assert_eq!(input, json!({"command": "rm -rf x"}));
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn unconfined_agents_require_approval_on_auto() {
        let dir = std::env::temp_dir().join(format!("grouper-gate-unconfined-{}", std::process::id()));
        let ws = Workspace::create(dir.clone()).unwrap();

        // hermes, grok, opencode: every *_chat asks on Auto with the unconfined reason;
        // Manual always asks; Bypass runs; list_* never asks on Auto.
        for (chat, list) in [
            ("hermes__hermes_chat", "hermes__list_hermes_chats"),
            ("grok__grok_chat", "grok__list_grok_chats"),
            ("opencode__opencode_chat", "opencode__list_opencode_chats"),
        ] {
            let mut input = json!({"message": "hi"});
            let reason = check_call(Permission::Auto, chat, &mut input, &ws).unwrap();
            assert!(reason.contains(agents::UNCONFINED_REASON), "{chat}: {reason}");

            let mut input = json!({"message": "hi"});
            let reason = check_call(Permission::Manual, chat, &mut input, &ws).unwrap();
            assert!(reason.contains("Manual"), "{chat}: {reason}");

            let mut input = json!({"message": "hi"});
            assert_eq!(check_call(Permission::Bypass, chat, &mut input, &ws), None, "{chat}");

            let mut input = json!({});
            assert_eq!(check_call(Permission::Auto, list, &mut input, &ws), None, "{list}");
            let mut input = json!({});
            assert_eq!(check_call(Permission::Bypass, list, &mut input, &ws), None, "{list}");
            let mut input = json!({});
            assert!(check_call(Permission::Manual, list, &mut input, &ws).unwrap().contains("Manual"), "{list}");
        }

        // opencode with auto_approve: false still asks on Auto.
        let mut input = json!({"message": "hi", "auto_approve": false});
        let reason = check_call(Permission::Auto, "opencode__opencode_chat", &mut input, &ws).unwrap();
        assert!(reason.contains(agents::UNCONFINED_REASON), "{reason}");

        // pi: Auto asks when tools is "all", missing, or invalid (removed by clamp);
        // runs without asking with "read-only" or "none".
        let pi = "pi__pi_chat";
        for args in [
            json!({"message": "hi"}),
            json!({"message": "hi", "tools": "all"}),
            json!({"message": "hi", "tools": "everything"}),
        ] {
            let mut input = args.clone();
            let reason = check_call(Permission::Auto, pi, &mut input, &ws).unwrap();
            assert!(reason.contains(agents::UNCONFINED_REASON), "{args}: {reason}");
        }
        for safe in ["read-only", "none"] {
            let mut input = json!({"message": "hi", "tools": safe});
            assert_eq!(check_call(Permission::Auto, pi, &mut input, &ws), None, "pi {safe}");
            assert_eq!(input["tools"], safe);
            // Manual still asks even for read-only/none.
            let mut input = json!({"message": "hi", "tools": safe});
            assert!(check_call(Permission::Manual, pi, &mut input, &ws).unwrap().contains("Manual"));
            // Risky message or outside workspace still asks on Auto for read-only/none.
            let mut input = json!({"message": "git push origin main", "tools": safe});
            assert!(check_call(Permission::Auto, pi, &mut input, &ws).is_some());
        }
        // pi on Bypass: runs and sets tools: all when omitted.
        let mut input = json!({"message": "hi"});
        assert_eq!(check_call(Permission::Bypass, pi, &mut input, &ws), None);
        assert_eq!(input["tools"], "all");
        // pi list_* never asks on Auto.
        let mut input = json!({});
        assert_eq!(check_call(Permission::Auto, "pi__list_pi_chats", &mut input, &ws), None);

        // Confined agents still run on Auto without asking.
        for chat in [
            "codex__codex_chat",
            "claude-code__claude_code_chat",
            "antigravity__antigravity_chat",
            "cursor__cursor_chat",
        ] {
            let mut input = json!({"message": "Write tests"});
            assert_eq!(check_call(Permission::Auto, chat, &mut input, &ws), None, "{chat}");
            let mut input = json!({"message": "Write tests"});
            assert_eq!(check_call(Permission::Bypass, chat, &mut input, &ws), None, "{chat}");
            let mut input = json!({"message": "Write tests"});
            assert!(check_call(Permission::Manual, chat, &mut input, &ws).unwrap().contains("Manual"), "{chat}");
        }

        std::fs::remove_dir_all(dir).ok();
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

    #[test]
    fn windows_shell_rules() {
        let risky = [
            "Remove-Item -Recurse -Force build",
            "ri -Recurse foo",
            "del /f /q file.txt",
            "erase notes.txt",
            "rd /s /q build",
            "Clear-Content log.txt",
            "Clear-RecycleBin -Force",
            "Move-Item a.txt b.txt",
            "mi a.txt b.txt",
            "move a.txt b.txt",
            "ren a.txt b.txt",
            "Rename-Item a.txt b.txt",
            "Invoke-WebRequest https://example.com",
            "iwr https://example.com",
            "Invoke-RestMethod https://example.com/api",
            "irm https://example.com",
            "Start-BitsTransfer http://example.com/a",
            "Test-NetConnection example.com",
            "ftp ftp://example.com",
            "New-Object Net.WebClient",
            "New-Object System.Net.WebClient",
            "Invoke-Expression 'Get-Date'",
            "iex 'Get-Date'",
            "Start-Process notepad",
            "saps notepad",
            "start notepad",
            r"& .\script.ps1",
            r"& .\build.ps1",
            "powershell -Command Get-Date",
            "pwsh -Command Get-Date",
            "cmd /c dir",
            "cmd.exe /c dir",
            "Add-Type -AssemblyName System.Windows.Forms",
            "Set-ExecutionPolicy Bypass",
            r"reg add HKCU\Software\X",
            "regedit",
            r"Set-ItemProperty -Path HKCU:\Software -Name A -Value 1",
            r"New-ItemProperty -Path HKCU:\Software -Name A -Value 1",
            r"Remove-ItemProperty -Path HKCU:\Software -Name A",
            r"Get-ChildItem HKLM:\Software",
            "cd HKLM:",
            "sc.exe stop wuauserv",
            "sc query",
            "New-Service -Name X -BinaryPathName app.exe",
            "Set-Service -Name X -StartupType Automatic",
            "net user",
            "netsh advfirewall show allprofiles",
            "schtasks /create /tn x /tr y",
            "Register-ScheduledTask -TaskName x",
            "bcdedit /enum",
            "diskpart",
            "format C:",
            "icacls file /grant Everyone:F",
            "takeown /f file",
            "cacls file /e",
            "Stop-Process -Name notepad",
            "spps -Name notepad",
            "taskkill /IM notepad.exe /F",
            "Stop-Computer -Force",
            "Restart-Computer",
            "wsl -l -v",
            "winget install git",
            "choco install git",
            "scoop install git",
            "Install-Module PSReadLine",
            "Install-Package 7zip",
            "msiexec /i installer.msi",
            "npm install -g typescript",
            "pip install requests",
            "Out-File out.txt",
            "Set-Content out.txt hello",
            "Add-Content out.txt hello",
            "New-Item file.txt -ItemType File",
            r"Get-Content C:\Windows\win.ini",
            "cat D:/secret.txt",
            r"Get-ChildItem \\server\share",
            r"Get-Content $env:USERPROFILE\.ssh\id_rsa",
            r"Get-Content $env:APPDATA\secrets",
            r"type %USERPROFILE%\.ssh\id_rsa",
            "dir %APPDATA%",
            "REMOVE-ITEM -Force x",
            r"& 'C:\Windows\System32\calc.exe'",
            "curl.exe https://example.com",
            "shutdown /s /t 0",
            r"Get-Content ..\secret",
            r"Get-Content $HOME\.ssh\id_rsa",
            r"ls ~\Documents",
        ];
        assert!(risky.len() >= 25, "{}", risky.len());
        for cmd in risky {
            assert!(shell_risk(cmd).is_some(), "{cmd}");
        }

        let fine = [
            "Get-ChildItem",
            "ls -Name",
            "dir",
            "Get-Content a.txt",
            "cat a.txt",
            "type a.txt",
            "Select-String -Path a.txt -Pattern foo",
            "Select-String -Pattern https://example.com -Path a.txt",
            "Measure-Object",
            "Format-Table",
            "format-table -AutoSize",
            "FORMAT-TABLE",
            "Format-List",
            "Where-Object { $_.Name -like '*.rs' }",
            "cargo build",
            "cargo test",
            "cargo build 2>&1",
            "git status",
            "git diff",
            "git log",
            "git status && git diff",
            "python script.py",
            "Get-ChildItem env:",
            "$PSVersionTable.PSVersion.Major",
            "echo hello",
            "Write-Output hello",
            "Start-Sleep 1",
            "Get-Item .",
            "cd subdir",
            "echo $homepage",
            "npm install typescript",
        ];
        assert!(fine.len() >= 10, "{}", fine.len());
        for cmd in fine {
            let risk = shell_risk(cmd);
            assert!(risk.is_none(), "{cmd} => {risk:?}");
        }

        let msg = needs_approval(Permission::Auto, "run_shell", &json!({"command": "Remove-Item x"})).unwrap();
        assert!(msg.contains("Remove-Item"), "{msg}");
        assert!(needs_approval(Permission::Bypass, "run_shell", &json!({"command": "Remove-Item x"})).is_none());
    }
}
