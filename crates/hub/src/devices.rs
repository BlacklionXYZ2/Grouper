//! Choosing a device for a thread, the same way [`crate::registry`] chooses a model.
//!
//! The orchestrator can leave `device` off `start_thread` and pass `needs`. Online
//! devices that satisfy every need are ranked by their record for the task type
//! (at least [`MIN_ROUTING_RUNS`] finished threads and a success rate of 50% first,
//! then by suitability). The hub is the fallback only for file-only work. The
//! reason is stored on the thread, as model-routing reasons are.
//!
//! # Need tokens
//!
//! | Token | A device satisfies it when |
//! |---|---|
//! | `shell` | it offers `run_shell`. The hub never does: file tools only |
//! | `gpu` | an available tool's name contains `gpu` (a platform tool such as `gpu`) |
//! | `xcode` | its platform is macOS (`macos`, `mac`, `darwin`, `osx`), or a tool's name contains `xcode`. Not the hub |
//! | `folder:<name>` | it reported a scoped folder of that name (`folder:projects` is `@projects`) |
//! | `agent:<server>` | it offers an MCP tool from that server (`agent:codex` is `codex__codex_chat`) |
//! | `files` | it offers a workspace file tool, or it is the hub |
//!
//! Anything else is an error. Matching ignores case. Only tools the registry still
//! marks available count. The hub satisfies `files` and nothing else, whatever
//! platform it happens to run on.

use std::collections::HashMap;

use core_protocol::{FILE_TOOLS, OutcomeStats, Permission, mcp_server_of, valid_folder_name};
use serde::Serialize;
use serde_json::{Value, json};

use crate::registry::{By, MIN_ROUTING_RUNS, stats_line};
use crate::server::{HUB_DEVICE, Hub};

/// One capability the brief asks for. Built by [`parse_needs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Need {
    Files,
    Shell,
    Gpu,
    Xcode,
    Folder(String),
    Agent(String),
}

impl Need {
    fn token(&self) -> String {
        match self {
            Self::Files => "files".into(),
            Self::Shell => "shell".into(),
            Self::Gpu => "gpu".into(),
            Self::Xcode => "xcode".into(),
            Self::Folder(name) => format!("folder:{name}"),
            Self::Agent(name) => format!("agent:{name}"),
        }
    }
}

const KNOWN_NEEDS: &str = "shell, gpu, xcode, files, folder:<name>, agent:<name>";

/// Parse `start_thread` / `suggest_device` needs. Drops blanks, lower-cases, de-duplicates.
pub(crate) fn parse_needs(raw: &[String]) -> Result<Vec<Need>, String> {
    let mut out = Vec::new();
    for item in raw {
        let token = item.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        let need = parse_need(&token)?;
        if !out.contains(&need) {
            out.push(need);
        }
    }
    Ok(out)
}

fn parse_need(token: &str) -> Result<Need, String> {
    if let Some(name) = token.strip_prefix("folder:") {
        if !valid_folder_name(name) {
            return Err(format!(
                "folder:{name:?} is not a folder name (1-32 letters, digits, '_' or '-'); example folder:projects"
            ));
        }
        return Ok(Need::Folder(name.to_string()));
    }
    if let Some(name) = token.strip_prefix("agent:") {
        let ok = (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !ok {
            return Err(format!(
                "agent:{name:?} is not an MCP server name; example agent:codex"
            ));
        }
        return Ok(Need::Agent(name.to_string()));
    }
    match token {
        "files" => Ok(Need::Files),
        "shell" => Ok(Need::Shell),
        "gpu" => Ok(Need::Gpu),
        "xcode" => Ok(Need::Xcode),
        other => Err(format!(
            "unknown need {other:?}. Known needs: {KNOWN_NEEDS}"
        )),
    }
}

/// What the hub knows about a device when it matches needs. `tools` are available names.
#[derive(Debug, Clone)]
pub(crate) struct DeviceProfile {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub online: bool,
    pub hub: bool,
    pub tools: Vec<String>,
    pub folders: Vec<String>,
}

/// Proven (enough finished threads, success rate at least 50%), not enough record, or a poor record.
/// Ordered so proven sorts first, then unproven, then poor — the same tiers as model routing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Record {
    Proven,
    Unproven,
    Poor,
}

fn record_of(stats: &OutcomeStats) -> Record {
    let finished = stats.done + stats.failed;
    if finished < MIN_ROUTING_RUNS {
        return Record::Unproven;
    }
    if stats.success_rate.unwrap_or(0.0) >= 0.5 {
        Record::Proven
    } else {
        Record::Poor
    }
}

/// Suitability of one device for one task type. Higher is better.
///
/// No finished threads (`done + failed`; stopped threads do not count): `0.0`.
/// Otherwise
///
/// ```text
/// success_rate * 100 + mean_grade - ln(1 + median_duration_seconds)
/// ```
///
/// Grade is `0` when the threads were not graded. A missing duration counts as 0
/// seconds. The score is only compared inside a record tier: a proven device beats
/// an unproven one, and a poor record (under 50% after [`MIN_ROUTING_RUNS`]) ranks
/// last, even when its raw score is higher.
pub(crate) fn suitability_score(stats: Option<&OutcomeStats>) -> f64 {
    let Some(stats) = stats else { return 0.0 };
    if stats.done + stats.failed == 0 {
        return 0.0;
    }
    let rate = stats.success_rate.filter(|r| r.is_finite()).unwrap_or(0.0);
    let grade = stats.mean_grade.filter(|g| g.is_finite()).unwrap_or(0.0);
    let duration = stats
        .median_duration_s
        .filter(|d| d.is_finite() && *d >= 0.0)
        .unwrap_or(0.0);
    rate * 100.0 + grade - (1.0 + duration).ln()
}

fn is_mac(platform: &str) -> bool {
    matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "macos" | "mac" | "darwin" | "osx"
    )
}

fn tool_has(profile: &DeviceProfile, needle: &str) -> bool {
    profile
        .tools
        .iter()
        .any(|name| name.to_ascii_lowercase().contains(needle))
}

fn satisfies(profile: &DeviceProfile, need: &Need) -> bool {
    match need {
        Need::Files => {
            profile.hub
                || profile.tools.iter().any(|name| {
                    FILE_TOOLS
                        .iter()
                        .any(|tool| name.eq_ignore_ascii_case(tool))
                })
        }
        Need::Shell => {
            !profile.hub
                && profile
                    .tools
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case("run_shell"))
        }
        Need::Gpu => !profile.hub && tool_has(profile, "gpu"),
        Need::Xcode => !profile.hub && (is_mac(&profile.platform) || tool_has(profile, "xcode")),
        Need::Folder(name) => {
            !profile.hub
                && profile
                    .folders
                    .iter()
                    .any(|folder| folder.eq_ignore_ascii_case(name))
        }
        Need::Agent(server) => {
            !profile.hub
                && profile
                    .tools
                    .iter()
                    .any(|name| mcp_server_of(name).is_some_and(|s| s.eq_ignore_ascii_case(server)))
        }
    }
}

fn missing_needs(profile: &DeviceProfile, needs: &[Need]) -> Vec<Need> {
    needs
        .iter()
        .filter(|need| !satisfies(profile, need))
        .cloned()
        .collect()
}

/// Need tokens this device can honestly claim, in a stable order.
pub(crate) fn satisfied_tokens(profile: &DeviceProfile) -> Vec<String> {
    let mut out = Vec::new();
    for need in [Need::Files, Need::Shell, Need::Gpu, Need::Xcode] {
        if satisfies(profile, &need) {
            out.push(need.token());
        }
    }
    let mut folders: Vec<&str> = profile.folders.iter().map(String::as_str).collect();
    folders.sort_by_key(|name| name.to_ascii_lowercase());
    for name in folders {
        if !profile.hub {
            out.push(format!("folder:{}", name.to_ascii_lowercase()));
        }
    }
    if !profile.hub {
        let mut agents: Vec<String> = profile
            .tools
            .iter()
            .filter_map(|name| mcp_server_of(name).map(|server| server.to_ascii_lowercase()))
            .collect();
        agents.sort();
        agents.dedup();
        for server in agents {
            out.push(format!("agent:{server}"));
        }
    }
    out
}

fn explains(need: &Need) -> String {
    match need {
        Need::Files => "file tools".into(),
        Need::Shell => "the run_shell tool".into(),
        Need::Gpu => "a tool whose name contains \"gpu\"".into(),
        Need::Xcode => "macOS, or a tool whose name contains \"xcode\"".into(),
        Need::Folder(name) => format!("scoped folder @{name}"),
        Need::Agent(name) => format!("an MCP tool from server {name} ({name}__...)"),
    }
}

fn on_off(online: bool) -> &'static str {
    if online { "online" } else { "offline" }
}

/// Devices other than `except_id` that have every need, or a partial list when none do.
fn who_has(needs: &[Need], profiles: &[DeviceProfile], except_id: &str) -> String {
    let word = if needs.len() == 1 {
        "it"
    } else {
        "all of them"
    };
    let mut full: Vec<&DeviceProfile> = profiles
        .iter()
        .filter(|profile| profile.id != except_id && missing_needs(profile, needs).is_empty())
        .collect();
    full.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
            .then(a.id.cmp(&b.id))
    });
    if !full.is_empty() {
        let list = full
            .iter()
            .map(|profile| format!("{} ({})", profile.name, on_off(profile.online)))
            .collect::<Vec<_>>()
            .join(", ");
        return format!("Devices that have {word}: {list}.");
    }
    let mut partial: Vec<String> = profiles
        .iter()
        .filter(|profile| {
            profile.id != except_id && needs.iter().any(|need| satisfies(profile, need))
        })
        .map(|profile| {
            let has = needs
                .iter()
                .filter(|need| satisfies(profile, need))
                .map(Need::token)
                .collect::<Vec<_>>()
                .join(", ");
            format!("{} has {has} ({})", profile.name, on_off(profile.online))
        })
        .collect();
    partial.sort();
    if partial.is_empty() {
        format!("No device has {word}.")
    } else {
        format!("No device has {word}. {}.", partial.join("; "))
    }
}

fn lack_error(
    device_name: &str,
    missing: &[Need],
    needs: &[Need],
    profiles: &[DeviceProfile],
    except_id: &str,
) -> String {
    let lacks = missing
        .iter()
        .map(|need| format!("{} ({})", need.token(), explains(need)))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{device_name} lacks {lacks}. {} Name one that has every need, or leave device out.",
        who_has(needs, profiles, except_id)
    )
}

fn file_only(needs: &[Need]) -> bool {
    needs.iter().all(|need| matches!(need, Need::Files))
}

fn none_fit_error(needs: &[Need], profiles: &[DeviceProfile]) -> String {
    if file_only(needs) {
        return "No online device is available for file-only work.".into();
    }
    let what = needs.iter().map(Need::token).collect::<Vec<_>>().join(", ");
    let mut msg = format!(
        "No online device satisfies {what}. {}",
        who_has(needs, profiles, "")
    );
    if profiles
        .iter()
        .any(|profile| !profile.hub && missing_needs(profile, needs).is_empty())
    {
        msg.push_str(" Name one in device to wait for it.");
    }
    if !profiles
        .iter()
        .any(|profile| profile.hub && missing_needs(profile, needs).is_empty())
    {
        msg.push_str(" The hub only does file work, so it can't take this.");
    }
    msg
}

fn choice_reason(
    name: &str,
    hub: bool,
    task_type: &str,
    needs: &[Need],
    stats: Option<&OutcomeStats>,
) -> String {
    let needs_bit = if needs.is_empty() {
        String::new()
    } else {
        format!(
            "; needs {}",
            needs.iter().map(Need::token).collect::<Vec<_>>().join(", ")
        )
    };
    let body = if hub {
        "no other online device fits; file-only work runs on the hub".to_string()
    } else if let Some(stats) = stats.filter(|stats| record_of(stats) == Record::Proven) {
        let grade = stats
            .mean_grade
            .map(|grade| format!(", grade {grade:.1}"))
            .unwrap_or_default();
        let duration = stats
            .median_duration_s
            .map(|secs| format!(", {secs:.0}s median"))
            .unwrap_or_default();
        format!(
            "best {task_type} record ({}/{} succeeded{grade}{duration})",
            stats.successes,
            stats.done + stats.failed
        )
    } else if stats.is_some_and(|stats| record_of(stats) == Record::Poor) {
        format!("only online fit; its {task_type} record is below 50%")
    } else if task_type.is_empty() {
        "online and can do the work".into()
    } else {
        format!("online and can do the work; no proven {task_type} record yet")
    };
    format!("device {name}: {body}{needs_bit}")
}

fn row_reason(
    fits: bool,
    missing: &[Need],
    online: bool,
    hub: bool,
    record: Record,
    task_type: &str,
) -> String {
    if !fits {
        return format!(
            "lacks {}",
            missing
                .iter()
                .map(Need::token)
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !online {
        return "offline".into();
    }
    if hub {
        return "file tools only".into();
    }
    match record {
        Record::Proven => format!("proven for {task_type}"),
        Record::Unproven => {
            if task_type.is_empty() {
                "online".into()
            } else {
                format!("no proven {task_type} record yet")
            }
        }
        Record::Poor => format!("{task_type} record below 50%"),
    }
}

/// Where `start_thread` would run, or why it would refuse.
#[derive(Debug, Clone, Serialize)]
pub struct DevicePick {
    pub id: String,
    pub name: String,
    pub score: f64,
    pub reason: String,
}

/// One device in a [`DeviceSuggestion`], best first.
#[derive(Debug, Clone, Serialize)]
pub struct RankedDevice {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub online: bool,
    pub score: f64,
    pub fits: bool,
    pub missing: Vec<String>,
    pub satisfies: Vec<String>,
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<String>,
}

/// Read-only answer of `suggest_device`. `choice` is null when nothing can take the work.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceSuggestion {
    pub task_type: String,
    pub needs: Vec<String>,
    pub choice: Option<DevicePick>,
    pub ranked: Vec<RankedDevice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

struct Row {
    ranked: RankedDevice,
    hub: bool,
    online: bool,
    fits: bool,
    tier: Record,
    missing_count: usize,
    stats: Option<OutcomeStats>,
}

fn group_of(row: &Row) -> u8 {
    if row.fits && row.online && !row.hub {
        0
    } else if row.fits && row.hub {
        1
    } else if row.fits && !row.online {
        2
    } else {
        3
    }
}

/// Rank `profiles` for `task_type` and `needs`. `stats` are per-device rows; only the
/// matching task type is used. An empty task type does not consult stats.
pub(crate) fn suggest(
    profiles: &[DeviceProfile],
    stats: &[OutcomeStats],
    task_type: &str,
    needs: &[Need],
) -> DeviceSuggestion {
    let task_type = task_type.trim().to_ascii_lowercase();
    let by_device: HashMap<&str, &OutcomeStats> = stats
        .iter()
        .filter(|stats| !task_type.is_empty() && stats.task_type == task_type)
        .map(|stats| (stats.subject.as_str(), stats))
        .collect();
    let mut rows: Vec<Row> = profiles
        .iter()
        .map(|profile| {
            let stats = by_device.get(profile.id.as_str()).copied().cloned();
            let record = stats.as_ref().map(record_of).unwrap_or(Record::Unproven);
            let missing = missing_needs(profile, needs);
            let fits = missing.is_empty();
            Row {
                ranked: RankedDevice {
                    id: profile.id.clone(),
                    name: profile.name.clone(),
                    platform: profile.platform.clone(),
                    online: profile.online,
                    score: suitability_score(stats.as_ref()),
                    fits,
                    missing: missing.iter().map(Need::token).collect(),
                    satisfies: satisfied_tokens(profile),
                    reason: row_reason(
                        fits,
                        &missing,
                        profile.online,
                        profile.hub,
                        record,
                        &task_type,
                    ),
                    stats: stats.as_ref().map(stats_line),
                },
                hub: profile.hub,
                online: profile.online,
                fits,
                tier: record,
                missing_count: missing.len(),
                stats,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        group_of(a)
            .cmp(&group_of(b))
            .then_with(|| {
                if a.fits {
                    (a.tier as u8).cmp(&(b.tier as u8))
                } else {
                    a.missing_count.cmp(&b.missing_count)
                }
            })
            .then(b.ranked.score.total_cmp(&a.ranked.score))
            .then(a.ranked.id.cmp(&b.ranked.id))
    });
    let choice_at = rows
        .iter()
        .position(|row| group_of(row) == 0)
        .or_else(|| rows.iter().position(|row| group_of(row) == 1));
    let (choice, error) = if let Some(index) = choice_at {
        let reason = choice_reason(
            &rows[index].ranked.name,
            rows[index].hub,
            &task_type,
            needs,
            rows[index].stats.as_ref(),
        );
        rows[index].ranked.reason = reason.clone();
        let ranked = &rows[index].ranked;
        (
            Some(DevicePick {
                id: ranked.id.clone(),
                name: ranked.name.clone(),
                score: ranked.score,
                reason,
            }),
            None,
        )
    } else {
        (None, Some(none_fit_error(needs, profiles)))
    };
    DeviceSuggestion {
        task_type,
        needs: needs.iter().map(Need::token).collect(),
        choice,
        ranked: rows.into_iter().map(|row| row.ranked).collect(),
        error,
    }
}

impl Hub {
    pub(crate) async fn device_profiles(&self) -> Vec<DeviceProfile> {
        let mut out = Vec::new();
        for device in self.devices().await {
            let hub = device.id == HUB_DEVICE;
            let tools = if hub {
                FILE_TOOLS.iter().map(|name| (*name).to_string()).collect()
            } else {
                self.db
                    .tools(Some(&device.id))
                    .into_iter()
                    .map(|tool| tool.name)
                    .collect()
            };
            out.push(DeviceProfile {
                id: device.id,
                name: device.name,
                platform: device.platform,
                online: device.online,
                hub,
                tools,
                folders: device
                    .folders
                    .into_iter()
                    .map(|folder| folder.name)
                    .collect(),
            });
        }
        out
    }

    async fn suggest_parsed(&self, task_type: Option<&str>, needs: &[Need]) -> DeviceSuggestion {
        let task = task_type.unwrap_or("").trim().to_ascii_lowercase();
        let stats = if task.is_empty() {
            Vec::new()
        } else {
            self.db.outcome_stats(By::Device, None, Some(&task))
        };
        let profiles = self.device_profiles().await;
        suggest(&profiles, &stats, &task, needs)
    }

    /// Rank devices for a task. Does not start a thread. Unknown needs are an error;
    /// a brief no online device can take is `choice: null` plus `error`.
    pub async fn suggest_device(
        &self,
        task_type: Option<&str>,
        needs: &[String],
    ) -> Result<DeviceSuggestion, String> {
        let parsed = parse_needs(needs)?;
        Ok(self.suggest_parsed(task_type, &parsed).await)
    }

    /// `(device id, display name, routing reason)`. The reason is set only when the
    /// caller left the device unset and the hub chose.
    pub(crate) async fn resolve_thread_device(
        &self,
        requested: &str,
        task_type: Option<&str>,
        needs: &[String],
    ) -> Result<(String, String, Option<String>), String> {
        let parsed = parse_needs(needs)?;
        let requested = requested.trim();
        if requested.is_empty() {
            let suggestion = self.suggest_parsed(task_type, &parsed).await;
            let Some(pick) = suggestion.choice else {
                return Err(suggestion
                    .error
                    .unwrap_or_else(|| "no device fits this brief".into()));
            };
            return Ok((pick.id, pick.name, Some(pick.reason)));
        }
        let (id, name) = self
            .resolve_device(requested)
            .ok_or_else(|| format!("no device called {requested:?}"))?;
        let profiles = self.device_profiles().await;
        let Some(profile) = profiles.iter().find(|profile| profile.id == id) else {
            return Err(format!("no device called {requested:?}"));
        };
        let missing = missing_needs(profile, &parsed);
        if !missing.is_empty() {
            return Err(lack_error(&name, &missing, &parsed, &profiles, &id));
        }
        Ok((id, name, None))
    }

    /// `list_devices`: tools and permissions, scoped folders, outcome stats per task
    /// type, and the need tokens the device satisfies.
    pub(crate) async fn device_list_rows(&self) -> Vec<Value> {
        let profiles = self.device_profiles().await;
        let stats = self.db.outcome_stats(By::Device, None, None);
        let mut rows = Vec::new();
        for profile in &profiles {
            let thread_tools: Vec<Value> = if profile.hub {
                FILE_TOOLS
                    .iter()
                    .map(|name| json!({"name": name, "permission": Permission::Auto}))
                    .collect()
            } else {
                self.db
                    .tools(Some(&profile.id))
                    .into_iter()
                    .map(|tool| json!({"name": tool.name, "permission": tool.permission}))
                    .collect()
            };
            let mut row = json!({
                "id": profile.id,
                "name": profile.name,
                "platform": profile.platform,
                "online": profile.online,
                "thread_tools": thread_tools,
                "satisfies": satisfied_tokens(profile),
            });
            if !profile.hub {
                let folders = self.db.device_folders(&profile.id);
                if !folders.is_empty() {
                    row["scoped_folders"] = json!(
                        folders
                            .iter()
                            .map(|folder| json!({"name": format!("@{}", folder.name), "mode": folder.mode}))
                            .collect::<Vec<_>>()
                    );
                }
            }
            let lines: Vec<String> = stats
                .iter()
                .filter(|stats| stats.subject == profile.id && !stats.task_type.is_empty())
                .map(stats_line)
                .collect();
            if !lines.is_empty() {
                row["stats"] = json!(lines);
            }
            rows.push(row);
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::threads::ThreadRequest;
    use core_protocol::{FolderInfo, FolderMode, ModelEntry, ProviderConfig, ToolDef};

    fn profile(
        id: &str,
        name: &str,
        platform: &str,
        online: bool,
        tools: &[&str],
        folders: &[&str],
    ) -> DeviceProfile {
        DeviceProfile {
            id: id.into(),
            name: name.into(),
            platform: platform.into(),
            online,
            hub: false,
            tools: tools.iter().map(|tool| (*tool).to_string()).collect(),
            folders: folders.iter().map(|folder| (*folder).to_string()).collect(),
        }
    }

    fn hub_profile() -> DeviceProfile {
        DeviceProfile {
            id: HUB_DEVICE.into(),
            name: "Hub".into(),
            platform: "linux".into(),
            online: true,
            hub: true,
            tools: FILE_TOOLS.iter().map(|name| (*name).to_string()).collect(),
            folders: vec![],
        }
    }

    fn code_stats(
        subject: &str,
        successes: u64,
        failed: u64,
        grade: Option<f64>,
        duration: Option<f64>,
    ) -> OutcomeStats {
        let done = successes;
        let finished = done + failed;
        OutcomeStats {
            subject: subject.into(),
            task_type: "code".into(),
            done,
            failed,
            stopped: 0,
            successes,
            success_rate: (finished > 0).then(|| successes as f64 / finished as f64),
            median_duration_s: duration,
            mean_cost: None,
            mean_grade: grade,
            net_rating: 0,
        }
    }

    #[test]
    fn score_uses_success_grade_and_duration_inside_record_tiers() {
        let perfect = code_stats("a", 3, 0, Some(5.0), Some(0.0));
        assert!((suitability_score(Some(&perfect)) - 105.0).abs() < 1e-9);
        let slower = code_stats("b", 3, 0, Some(5.0), Some(std::f64::consts::E - 1.0));
        assert!(
            (suitability_score(Some(&slower)) - 104.0).abs() < 1e-6,
            "{}",
            suitability_score(Some(&slower))
        );
        assert!(suitability_score(Some(&perfect)) > suitability_score(Some(&slower)));

        let stopped_only = OutcomeStats {
            subject: "s".into(),
            task_type: "code".into(),
            stopped: 4,
            ..Default::default()
        };
        assert_eq!(suitability_score(Some(&stopped_only)), 0.0);
        assert_eq!(record_of(&stopped_only), Record::Unproven);

        // 60% of 3 is proven but scores under an unproven 2/2. The tier still wins.
        let proven = code_stats("pc", 2, 1, Some(3.0), Some(0.0));
        assert_eq!(record_of(&proven), Record::Proven);
        let unproven = code_stats("laptop", 2, 0, Some(5.0), Some(0.0));
        assert_eq!(record_of(&unproven), Record::Unproven);
        assert!(suitability_score(Some(&unproven)) > suitability_score(Some(&proven)));
        let poor = code_stats("mac", 0, 3, Some(1.0), Some(0.0));
        assert_eq!(record_of(&poor), Record::Poor);

        let profiles = vec![
            profile("laptop", "Laptop", "linux", true, &["run_shell"], &[]),
            profile("mac", "MacBook", "macos", true, &["run_shell"], &[]),
            profile("pc", "Gaming PC", "windows", true, &["run_shell"], &[]),
            hub_profile(),
        ];
        let suggestion = suggest(
            &profiles,
            &[proven, unproven, poor],
            "code",
            &parse_needs(&["shell".into()]).unwrap(),
        );
        let ids: Vec<&str> = suggestion
            .ranked
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(ids, vec!["pc", "laptop", "mac", "hub"]);
        let choice = suggestion.choice.expect("a device fits");
        assert_eq!(choice.id, "pc");
        assert!(
            choice.reason.contains("best code record") && choice.reason.contains("2/3 succeeded"),
            "{}",
            choice.reason
        );
        assert!(suggestion.error.is_none());
    }

    #[test]
    fn needs_map_tools_folders_platform_and_agents() {
        let parsed = parse_needs(&[
            " Shell ".into(),
            "shell".into(),
            "GPU".into(),
            "Folder:Projects".into(),
            "Agent:Codex".into(),
        ])
        .unwrap();
        assert_eq!(
            parsed,
            vec![
                Need::Shell,
                Need::Gpu,
                Need::Folder("projects".into()),
                Need::Agent("codex".into())
            ]
        );
        assert!(
            parse_needs(&["cuda".into()])
                .unwrap_err()
                .contains("unknown need")
        );
        assert!(
            parse_needs(&["folder:".into()])
                .unwrap_err()
                .contains("folder name")
        );
        assert!(
            parse_needs(&["agent:".into()])
                .unwrap_err()
                .contains("MCP server")
        );

        let mac = profile(
            "mac",
            "MacBook",
            "macos",
            true,
            &["read_file", "run_shell", "codex__codex_chat"],
            &["Projects"],
        );
        assert_eq!(
            satisfied_tokens(&mac),
            vec![
                "files".to_string(),
                "shell".into(),
                "xcode".into(),
                "folder:projects".into(),
                "agent:codex".into()
            ]
        );
        // No file tool: files is not claimed. A Windows box with xcodebuild still has xcode.
        let win = profile(
            "pc",
            "Gaming PC",
            "Windows",
            false,
            &["GPU_info", "xcodebuild"],
            &[],
        );
        assert!(satisfies(&win, &Need::Gpu));
        assert!(satisfies(&win, &Need::Xcode));
        assert!(!satisfies(&win, &Need::Shell));
        assert!(!satisfies(&win, &Need::Files));
        assert!(!satisfies(
            &profile("pc", "PC", "windows", true, &["codex_chat"], &[]),
            &Need::Agent("codex".into())
        ));

        let mut hub = hub_profile();
        hub.platform = "macos".into();
        hub.tools.extend([
            "run_shell".into(),
            "gpu".into(),
            "xcodebuild".into(),
            "codex__codex_chat".into(),
        ]);
        hub.folders.push("projects".into());
        assert_eq!(satisfied_tokens(&hub), vec!["files".to_string()]);
    }

    #[test]
    fn errors_name_devices_that_have_the_need() {
        let profiles = vec![
            profile("mac", "MacBook", "macos", true, &["run_shell"], &[]),
            profile(
                "pc",
                "Gaming PC",
                "windows",
                true,
                &["run_shell", "gpu"],
                &[],
            ),
            profile("lap", "Laptop", "linux", false, &["gpu"], &[]),
            hub_profile(),
        ];
        let needs = parse_needs(&["shell".into(), "gpu".into()]).unwrap();
        let mac = &profiles[0];
        let missing = missing_needs(mac, &needs);
        assert_eq!(
            lack_error(&mac.name, &missing, &needs, &profiles, &mac.id),
            "MacBook lacks gpu (a tool whose name contains \"gpu\"). Devices that have all of them: Gaming PC (online). \
             Name one that has every need, or leave device out."
        );
        let offline = vec![
            profile("pc", "Gaming PC", "windows", false, &["gpu"], &[]),
            hub_profile(),
        ];
        let gpu = parse_needs(&["gpu".into()]).unwrap();
        assert_eq!(
            none_fit_error(&gpu, &offline),
            "No online device satisfies gpu. Devices that have it: Gaming PC (offline). Name one in device to wait for it. \
             The hub only does file work, so it can't take this."
        );
        let split = vec![
            profile("pc", "Gaming PC", "windows", true, &["gpu"], &[]),
            profile("lap", "Laptop", "linux", false, &["run_shell"], &[]),
            hub_profile(),
        ];
        let err = none_fit_error(&needs, &split);
        assert!(
            err.contains("No device has all of them")
                && err.contains("Gaming PC has gpu")
                && err.contains("file work"),
            "{err}"
        );

        let idle = vec![
            profile("mac", "MacBook", "macos", false, &["run_shell"], &[]),
            hub_profile(),
        ];
        let suggestion = suggest(&idle, &[], "", &[]);
        let choice = suggestion.choice.as_ref().expect("hub fits file-only work");
        assert_eq!(choice.id, "hub");
        assert!(choice.reason.contains("file-only"), "{suggestion:?}");
    }

    fn tmp() -> std::path::PathBuf {
        std::env::temp_dir().join(format!("grouper-devices-{}", crate::db::random_hex(4)))
    }

    fn model() -> ModelEntry {
        ModelEntry {
            id: "m".into(),
            provider_id: "p".into(),
            model: "m-1".into(),
            input_price: 0.0,
            output_price: 0.0,
            cache_read_price: None,
            cache_write_price: None,
            context_window: 0,
            use_cases: "code".into(),
            free: true,
            roles: vec!["thread".into()],
            local: false,
            modalities: vec![],
        }
    }

    fn outcome(db: &Db, device: &str, status: &str, grade: Option<i64>) {
        let thread = db
            .create_thread("c1", "t", device, "m", None, "brief")
            .unwrap();
        db.set_thread_meta(&thread.id, "code", &[]);
        assert!(db.finish_thread(&thread.id, status, "s"));
        assert!(db.record_outcome(&thread.id));
        if let Some(grade) = grade {
            db.grade_thread(&thread.id, grade, "note").unwrap();
        }
    }

    #[test]
    fn ranking_reads_recorded_outcomes() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"default": [{"type": "text", "text": "OK"}]}}),
        })
        .unwrap();
        db.upsert_model(&model()).unwrap();
        for device in ["fast", "slow"] {
            for _ in 0..3 {
                outcome(&db, device, "done", Some(5));
            }
        }
        for _ in 0..2 {
            outcome(&db, "laptop", "done", Some(5));
        }
        for _ in 0..3 {
            outcome(&db, "mac", "failed", None);
        }
        db.c()
            .execute(
                "UPDATE thread_outcomes SET duration_s = 10 WHERE device_id = 'fast'",
                [],
            )
            .unwrap();
        db.c()
            .execute(
                "UPDATE thread_outcomes SET duration_s = 500 WHERE device_id = 'slow'",
                [],
            )
            .unwrap();

        let stats = db.outcome_stats(By::Device, None, Some("code"));
        let profiles = vec![
            profile("slow", "Slow", "linux", true, &["run_shell"], &[]),
            profile("fast", "Fast", "linux", true, &["run_shell"], &[]),
            profile("laptop", "Laptop", "linux", true, &["run_shell"], &[]),
            profile("mac", "Mac", "macos", true, &["run_shell"], &[]),
            hub_profile(),
        ];
        let suggestion = suggest(
            &profiles,
            &stats,
            "Code",
            &parse_needs(&["shell".into()]).unwrap(),
        );
        let ids: Vec<&str> = suggestion
            .ranked
            .iter()
            .map(|row| row.id.as_str())
            .collect();
        assert_eq!(
            ids,
            vec!["fast", "slow", "laptop", "mac", "hub"],
            "{suggestion:?}"
        );
        assert!(
            suggestion
                .choice
                .as_ref()
                .unwrap()
                .reason
                .contains("grade 5.0"),
            "{suggestion:?}"
        );
        assert!(suggestion.ranked[0].score > suggestion.ranked[1].score);
        std::fs::remove_dir_all(dir).ok();
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: name.into(),
            input_schema: json!({"type": "object"}),
        }
    }

    #[tokio::test]
    async fn list_rows_show_stats_and_satisfied_needs() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"default": [{"type": "text", "text": "OK"}]}}),
        })
        .unwrap();
        db.upsert_model(&model()).unwrap();
        let (mac, _) = db.add_device("MacBook", "macos", false).unwrap();
        db.upsert_tools(
            &mac.id,
            &[
                tool("run_shell"),
                tool("read_file"),
                tool("codex__codex_chat"),
            ],
        );
        db.set_device_folders(
            &mac.id,
            &[FolderInfo {
                name: "projects".into(),
                mode: FolderMode::ReadWrite,
            }],
        );
        outcome(&db, &mac.id, "done", Some(4));
        let hub = Hub::new(db, dir.clone());
        let rows = hub.device_list_rows().await;
        let mac_row = rows.iter().find(|row| row["name"] == "MacBook").unwrap();
        assert_eq!(mac_row["online"], false);
        assert_eq!(
            mac_row["satisfies"],
            json!(["files", "shell", "xcode", "folder:projects", "agent:codex"])
        );
        assert_eq!(
            mac_row["scoped_folders"],
            json!([{"name": "@projects", "mode": "read_write"}])
        );
        assert!(
            mac_row["stats"][0]
                .as_str()
                .unwrap()
                .starts_with("code: 1/1 ok"),
            "{}",
            mac_row["stats"]
        );
        let hub_row = rows.iter().find(|row| row["id"] == "hub").unwrap();
        assert_eq!(hub_row["satisfies"], json!(["files"]));
        assert_eq!(hub_row["online"], true);
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    fn scripted(db: &Db) {
        db.upsert_provider(&ProviderConfig {
            id: "p".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"inline": {"default": [{"type": "text", "text": "OK"}]}}),
        })
        .unwrap();
        let mut entry = model();
        entry.roles = vec!["orchestrator".into(), "thread".into()];
        db.upsert_model(&entry).unwrap();
    }

    #[tokio::test]
    async fn start_thread_errors_when_the_named_device_lacks_a_need_and_falls_back_to_the_hub() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        scripted(&db);
        let (mac, _) = db.add_device("MacBook", "macos", false).unwrap();
        let (pc, _) = db.add_device("Gaming PC", "windows", false).unwrap();
        db.upsert_tools(&pc.id, &[tool("gpu"), tool("run_shell")]);
        db.upsert_tools(&mac.id, &[tool("run_shell")]);
        let hub = Hub::new(db, dir.clone());
        let conv = hub.db.create_conversation("project", "devices").unwrap();

        let err = hub
            .start_thread(
                &conv,
                ThreadRequest {
                    title: "Train".into(),
                    device: "MacBook".into(),
                    goal: "Use the GPU.".into(),
                    needs: vec!["gpu".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert_eq!(
            err,
            "MacBook lacks gpu (a tool whose name contains \"gpu\"). Devices that have it: Gaming PC (offline). \
             Name one that has every need, or leave device out."
        );
        assert!(
            hub.db.threads(Some(&conv)).is_empty(),
            "a refused start records no thread"
        );

        let err = hub
            .start_thread(
                &conv,
                ThreadRequest {
                    title: "Build".into(),
                    goal: "Compile.".into(),
                    needs: vec!["shell".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(
            err.contains("No online device satisfies shell") && err.contains("Gaming PC (offline)"),
            "{err}"
        );
        assert!(
            err.contains("MacBook (offline)") && err.contains("file work"),
            "{err}"
        );

        let err = hub
            .start_thread(
                &conv,
                ThreadRequest {
                    title: "T".into(),
                    goal: "G".into(),
                    needs: vec!["cuda".into()],
                    ..Default::default()
                },
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown need \"cuda\""), "{err}");

        let thread = hub
            .start_thread(
                &conv,
                ThreadRequest {
                    title: "Notes".into(),
                    goal: "Write the notes.".into(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(thread.device_id, "hub");
        let reason = thread.reason.unwrap();
        assert!(
            reason.contains(
                "device Hub: no other online device fits; file-only work runs on the hub"
            ),
            "{reason}"
        );
        assert!(reason.contains("model m:"), "{reason}");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while hub
                .db
                .thread(&thread.id)
                .is_some_and(|thread| thread.status == "running")
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub thread finishes");
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
