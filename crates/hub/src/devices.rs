//! Device selection hub glue and install-level hub-candidate listing:
//! - Builds [`DeviceProfile`]s from live presence and the tool/folder registry, delegating
//!   ranking and need matching to [`core_registry::devices`].
//! - Groups active device rows by `install_id` and exposes `GET /v1/devices/installs` so
//!   clients can show which installs are eligible to become the hub (section 4a of
//!   `docs/local-first-design.md`).

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use core_protocol::{FILE_TOOLS, NodeHostInfo, Permission};
pub use core_registry::devices::{DevicePick, DeviceSuggestion, RankedDevice};
use core_registry::devices::{DeviceProfile, Need, lack_error, missing_needs, parse_needs, satisfied_tokens, suggest_from_conn};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::registry::{By, stats_line};
use crate::server::{HUB_DEVICE, Hub, err};

const HUB_VERSION: &str = env!("CARGO_PKG_VERSION");
const HOST_META_PREFIX: &str = "device_host:";
const HUB_SINCE_KEY: &str = "hub_since";

/// One underlying device row inside an [`InstallDevice`] group.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InstallDeviceRow {
    pub id: String,
    pub name: String,
    pub platform: String,
    pub admin: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_kind: Option<String>,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    pub is_hub: bool,
}

/// One install-level device entry returned by `GET /v1/devices/installs`.
/// An app's admin client row and plain node row share an `install_id` and appear once.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InstallDevice {
    /// Representative device id (the node row if present, else the hub row, else the first row).
    pub device_id: String,
    /// All non-revoked device row ids belonging to this install.
    pub device_ids: Vec<String>,
    /// All non-revoked device rows belonging to this install (admin UI row, node row, etc.).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<InstallDeviceRow>,
    /// The node device id that accepts WebSocket connections and move tickets, if present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_device_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    pub name: String,
    pub platform: String,
    pub is_hub: bool,
    pub online: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<NodeHostInfo>,
    /// True when the platform can host (`macos`, `windows`, `linux`) and the node reported `can_host: true`
    /// (or this install is the current hub).
    pub eligible: bool,
    /// True when this install can be picked as a hub-move target right now.
    pub selectable: bool,
    /// Machine-readable reason when `selectable` is `false`:
    /// `current_hub`, `cannot_host`, `offline`, or `older_version`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Non-blocking advisories (e.g. missing `cloudflared` or low disk headroom).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// Response payload for `GET /v1/devices/installs`.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct InstallDevicesResponse {
    pub hub_device_id: String,
    pub epoch: u64,
    pub hub_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_since: Option<u64>,
    pub estimated_snapshot_bytes: u64,
    pub installs: Vec<InstallDevice>,
}

impl InstallDevicesResponse {
    /// Installs eligible and ready to be offered as hub-move targets right now (never includes the hub's own install).
    pub fn move_targets(&self) -> Vec<&InstallDevice> {
        self.installs.iter().filter(|d| !d.is_hub && d.selectable).collect()
    }
}

#[derive(Clone, Debug)]
struct DeviceRow {
    id: String,
    name: String,
    platform: String,
    last_seen: Option<u64>,
    admin: bool,
    install_id: Option<String>,
    install_kind: Option<String>,
    created: u64,
}

/// Platforms that can run the Grouper hub (`macos`, `windows`, `linux`).
pub fn is_host_platform(platform: &str) -> bool {
    matches!(platform.trim().to_ascii_lowercase().as_str(), "macos" | "windows" | "linux")
}

/// Compare two semver-like version strings (`major.minor.patch`, ignoring any `-pre`/`+build` suffix).
pub(crate) fn compare_versions(left: &str, right: &str) -> Ordering {
    fn parse(v: &str) -> (u64, u64, u64) {
        let core = v.trim().trim_start_matches('v').split(['-', '+']).next().unwrap_or("");
        let mut parts = core.split('.');
        let major = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let minor = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let patch = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        (major, minor, patch)
    }
    parse(left).cmp(&parse(right))
}

/// Estimate the uncompressed size of `grouper.db` (plus WAL) and `workspaces/`.
pub(crate) fn estimate_snapshot_bytes(data_dir: &std::path::Path) -> u64 {
    fn dir_bytes(path: &std::path::Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        let mut total = 0u64;
        for entry in entries.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            }
            if ft.is_file() {
                total = total.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            } else if ft.is_dir() {
                total = total.saturating_add(dir_bytes(&entry.path()));
            }
        }
        total
    }
    let mut bytes = 0u64;
    for name in ["grouper.db", "grouper.db-wal"] {
        if let Ok(meta) = std::fs::metadata(data_dir.join(name)) {
            bytes = bytes.saturating_add(meta.len());
        }
    }
    bytes.saturating_add(dir_bytes(&data_dir.join("workspaces")))
}

pub(crate) fn routes() -> Router<Arc<Hub>> {
    Router::new().route("/v1/devices/installs", get(list_install_devices))
}

async fn list_install_devices(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Response {
    if hub.auth(&headers).is_none() {
        return err(StatusCode::UNAUTHORIZED, "invalid or missing device token");
    }
    Json(hub.install_devices().await).into_response()
}

impl Hub {
    pub(crate) async fn device_profiles(&self) -> Vec<DeviceProfile> {
        let hosts: Vec<String> = self
            .db
            .providers()
            .unwrap_or_default()
            .iter()
            .filter_map(|p| p.via_device().map(str::to_ascii_lowercase))
            .collect();
        let mut out = Vec::new();
        for device in self.devices().await {
            let hub = device.id == HUB_DEVICE;
            let tools = if hub {
                FILE_TOOLS.iter().map(|name| (*name).to_string()).collect()
            } else {
                self.db.tools(Some(&device.id)).into_iter().map(|tool| tool.name).collect()
            };
            let hosts_models = !hub
                && hosts.iter().any(|h| *h == device.id.to_ascii_lowercase() || *h == device.name.to_ascii_lowercase());
            out.push(DeviceProfile {
                hosts_models,
                id: device.id,
                name: device.name,
                platform: device.platform,
                online: device.online,
                hub,
                tools,
                folders: device.folders.into_iter().map(|folder| folder.name).collect(),
            });
        }
        out
    }

    async fn suggest_parsed(&self, task_type: Option<&str>, needs: &[Need]) -> DeviceSuggestion {
        let profiles = self.device_profiles().await;
        suggest_from_conn(&self.db.c(), &profiles, task_type, needs)
    }

    /// Rank devices for a task. Does not start a thread. Unknown needs are an error;
    /// a brief no online device can take is `choice: null` plus `error`.
    pub async fn suggest_device(&self, task_type: Option<&str>, needs: &[String]) -> Result<DeviceSuggestion, String> {
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
                return Err(suggestion.error.unwrap_or_else(|| "no device fits this brief".into()));
            };
            return Ok((pick.id, pick.name, Some(pick.reason)));
        }
        let (id, name) = self.resolve_device(requested).ok_or_else(|| format!("no device called {requested:?}"))?;
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
                FILE_TOOLS.iter().map(|name| json!({"name": name, "permission": Permission::Auto})).collect()
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

    /// Record host capability metadata reported in a device's `NodeMessage::Hello` frame.
    /// Cached in memory and persisted in `hub_meta` so offline installs remember whether they can host.
    pub fn record_host_info(&self, device_id: &str, host: NodeHostInfo) {
        if let Ok(json) = serde_json::to_string(&host) {
            let key = format!("{HOST_META_PREFIX}{device_id}");
            let _ = self.db.c().execute(
                "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
                params![key, json],
            );
        }
        self.host_info.lock().unwrap_or_else(|e| e.into_inner()).insert(device_id.to_string(), host);
    }

    /// Last-known host metadata for `device_id` (memory cache first, then `hub_meta`).
    pub fn host_info_for(&self, device_id: &str) -> Option<NodeHostInfo> {
        if let Some(info) = self.host_info.lock().unwrap_or_else(|e| e.into_inner()).get(device_id).cloned() {
            return Some(info);
        }
        let key = format!("{HOST_META_PREFIX}{device_id}");
        let raw: Option<String> = self
            .db
            .c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()
            .flatten();
        let parsed: NodeHostInfo = serde_json::from_str(&raw?).ok()?;
        self.host_info.lock().unwrap_or_else(|e| e.into_inner()).insert(device_id.to_string(), parsed.clone());
        Some(parsed)
    }

    /// Record when the current device became the hub (`hub_since` in `hub_meta`).
    pub(crate) fn record_hub_since(&self, ts: u64) {
        let _ = self.db.c().execute(
            "INSERT OR REPLACE INTO hub_meta (key, value) VALUES (?1, ?2)",
            params![HUB_SINCE_KEY, ts.to_string()],
        );
    }

    fn hub_since(&self, hub_device_created: Option<u64>) -> Option<u64> {
        let stored: Option<u64> = self
            .db
            .c()
            .query_row("SELECT value FROM hub_meta WHERE key = ?1", [HUB_SINCE_KEY], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok());
        stored.or(hub_device_created)
    }

    /// List devices grouped by install (`install_id`), reporting hub eligibility, online state,
    /// version compatibility, and non-blocking advisories (`GET /v1/devices/installs`).
    pub async fn install_devices(&self) -> InstallDevicesResponse {
        let (hub_device_id, epoch) = self.db.ensure_hub_device().unwrap_or_else(|_| (String::new(), 1));
        let estimated_snapshot_bytes = estimate_snapshot_bytes(&self.data_dir);
        let rows: Vec<DeviceRow> = {
            let c = self.db.c();
            let mut st = c
                .prepare(
                    "SELECT id, name, platform, last_seen, admin, install_id, install_kind, created
                     FROM devices WHERE revoked = 0 ORDER BY created",
                )
                .unwrap();
            st.query_map([], |r| {
                Ok(DeviceRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    platform: r.get(2)?,
                    last_seen: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
                    admin: r.get::<_, i64>(4)? != 0,
                    install_id: r.get::<_, Option<String>>(5)?.filter(|s| !s.is_empty()),
                    install_kind: r.get::<_, Option<String>>(6)?.filter(|s| !s.is_empty()),
                    created: r.get::<_, i64>(7)? as u64,
                })
            })
            .unwrap()
            .filter_map(Result::ok)
            .collect()
        };

        let hub_device_created = rows.iter().find(|r| r.id == hub_device_id).map(|r| r.created);
        let hub_since = self.hub_since(hub_device_created);

        // Group by canonical install_id when present (normalizing legacy `<install_id>-node` rows
        // so an app's admin, node, and hub rows always collapse into one install); rows without an
        // install_id stay separate.
        let mut groups: BTreeMap<String, Vec<DeviceRow>> = BTreeMap::new();
        for row in rows {
            let key = match row.install_id.as_deref() {
                Some(install) => {
                    let canon = crate::db::canonical_install_id(install, row.install_kind.as_deref(), row.admin);
                    format!("i:{canon}")
                }
                None => format!("d:{}", row.id),
            };
            groups.entry(key).or_default().push(row);
        }

        let live = self.live.lock().await;
        let local_host = NodeHostInfo {
            can_host: true,
            app_version: HUB_VERSION.to_string(),
            free_bytes: None,
            cloudflared: crate::tunnel::find_cloudflared().is_some(),
        };

        let mut installs = Vec::with_capacity(groups.len());
        for (_, group) in groups {
            let is_hub = group.iter().any(|r| r.id == hub_device_id);
            let install_id = group
                .iter()
                .find_map(|r| {
                    let raw = r.install_id.as_deref()?;
                    let canon = crate::db::canonical_install_id(raw, r.install_kind.as_deref(), r.admin);
                    (raw == canon).then(|| raw.to_string())
                })
                .or_else(|| {
                    group.iter().find_map(|r| {
                        let raw = r.install_id.as_deref()?;
                        Some(crate::db::canonical_install_id(raw, r.install_kind.as_deref(), r.admin).to_string())
                    })
                });
            let device_ids: Vec<String> = group.iter().map(|r| r.id.clone()).collect();
            let install_rows: Vec<InstallDeviceRow> = group
                .iter()
                .map(|r| InstallDeviceRow {
                    id: r.id.clone(),
                    name: r.name.clone(),
                    platform: r.platform.clone(),
                    admin: r.admin,
                    install_kind: r.install_kind.clone(),
                    online: live.contains_key(&r.id),
                    last_seen: r.last_seen,
                    is_hub: r.id == hub_device_id,
                })
                .collect();

            // Pick the node row (plain device or install_kind == "node") for WebSocket / move tickets.
            let node_row = group
                .iter()
                .find(|r| !r.admin && live.contains_key(&r.id))
                .or_else(|| group.iter().find(|r| r.install_kind.as_deref() == Some(crate::db::NODE_INSTALL)))
                .or_else(|| group.iter().find(|r| !r.admin));
            let node_device_id = node_row.map(|r| r.id.clone());

            let rep = if is_hub {
                group.iter().find(|r| r.id == hub_device_id).unwrap_or(&group[0])
            } else {
                node_row.unwrap_or(&group[0])
            };

            let online = is_hub || group.iter().any(|r| live.contains_key(&r.id));
            let last_seen = if is_hub {
                Some(crate::db::now())
            } else {
                group.iter().filter_map(|r| r.last_seen).max()
            };

            let host = if is_hub {
                Some(local_host.clone())
            } else {
                group.iter().find_map(|r| self.host_info_for(&r.id))
            };

            // Pick a desktop platform if any row in the install has one (e.g. macos + node).
            let platform = group
                .iter()
                .find(|r| is_host_platform(&r.platform))
                .map(|r| r.platform.clone())
                .unwrap_or_else(|| rep.platform.clone());

            let eligible = is_hub
                || (is_host_platform(&platform)
                    && host.as_ref().is_some_and(|h| h.can_host)
                    && node_device_id.is_some());
            let version_older = !is_hub
                && host
                    .as_ref()
                    .is_some_and(|h| compare_versions(&h.app_version, HUB_VERSION) == Ordering::Less);

            let (selectable, reason) = if is_hub {
                (false, Some("current_hub".to_string()))
            } else if !eligible {
                (false, Some("cannot_host".to_string()))
            } else if !online {
                (false, Some("offline".to_string()))
            } else if version_older {
                (false, Some("older_version".to_string()))
            } else {
                (true, None)
            };

            let mut warnings = Vec::new();
            if eligible && !is_hub && let Some(h) = &host {
                if !h.cloudflared {
                    warnings.push(
                        "Remote access will stop until cloudflared is installed there".to_string(),
                    );
                }
                let needed = estimated_snapshot_bytes.saturating_mul(2);
                if let Some(free) = h.free_bytes && free < needed {
                    warnings.push(format!(
                        "Low disk space: {} bytes free, need at least {} bytes for the move",
                        free, needed
                    ));
                }
            }

            installs.push(InstallDevice {
                device_id: rep.id.clone(),
                device_ids,
                rows: install_rows,
                node_device_id,
                install_id,
                name: rep.name.clone(),
                platform,
                is_hub,
                online,
                last_seen,
                host,
                eligible,
                selectable,
                reason,
                warnings,
            });
        }

        // Order: current hub first, then selectable, then eligible (offline/older), then ineligible; tie-break by name.
        installs.sort_by(|a, b| {
            let rank = |d: &InstallDevice| -> u8 {
                if d.is_hub {
                    0
                } else if d.selectable {
                    1
                } else if d.eligible {
                    2
                } else {
                    3
                }
            };
            rank(a)
                .cmp(&rank(b))
                .then_with(|| a.name.to_ascii_lowercase().cmp(&b.name.to_ascii_lowercase()))
                .then_with(|| a.device_id.cmp(&b.device_id))
        });

        InstallDevicesResponse {
            hub_device_id,
            epoch,
            hub_version: HUB_VERSION.to_string(),
            hub_since,
            estimated_snapshot_bytes,
            installs,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;
    use crate::threads::ThreadRequest;
    use core_protocol::{FolderInfo, FolderMode, ModelEntry, ProviderConfig, ToolDef};
    use core_registry::devices::suggest;

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
            hosts_models: false,
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
            hosts_models: false,
        }
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
            ..Default::default()
        }
    }

    fn outcome(db: &Db, device: &str, status: &str, grade: Option<i64>) {
        let thread = db.create_thread("c1", "t", device, "m", None, "brief").unwrap();
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
        db.c().execute("UPDATE thread_outcomes SET duration_s = 10 WHERE device_id = 'fast'", []).unwrap();
        db.c().execute("UPDATE thread_outcomes SET duration_s = 500 WHERE device_id = 'slow'", []).unwrap();

        let stats = db.outcome_stats(By::Device, None, Some("code"));
        let profiles = vec![
            profile("slow", "Slow", "linux", true, &["run_shell"], &[]),
            profile("fast", "Fast", "linux", true, &["run_shell"], &[]),
            profile("laptop", "Laptop", "linux", true, &["run_shell"], &[]),
            profile("mac", "Mac", "macos", true, &["run_shell"], &[]),
            hub_profile(),
        ];
        let suggestion = suggest(&profiles, &stats, "Code", &parse_needs(&["shell".into()]).unwrap());
        let ids: Vec<&str> = suggestion.ranked.iter().map(|row| row.id.as_str()).collect();
        assert_eq!(ids, vec!["fast", "slow", "laptop", "mac", "hub"], "{suggestion:?}");
        assert!(suggestion.choice.as_ref().unwrap().reason.contains("grade 5.0"), "{suggestion:?}");
        assert!(suggestion.ranked[0].score > suggestion.ranked[1].score);
        std::fs::remove_dir_all(dir).ok();
    }

    fn tool(name: &str) -> ToolDef {
        ToolDef { name: name.into(), description: name.into(), input_schema: json!({"type": "object"}) }
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
        db.upsert_tools(&mac.id, &[tool("run_shell"), tool("read_file"), tool("codex__codex_chat")]);
        db.set_device_folders(&mac.id, &[FolderInfo { name: "projects".into(), mode: FolderMode::ReadWrite }]);
        outcome(&db, &mac.id, "done", Some(4));
        let hub = Hub::new(db, dir.clone());
        let rows = hub.device_list_rows().await;
        let mac_row = rows.iter().find(|row| row["name"] == "MacBook").unwrap();
        assert_eq!(mac_row["online"], false);
        assert_eq!(mac_row["satisfies"], json!(["files", "shell", "xcode", "folder:projects", "agent:codex"]));
        assert_eq!(mac_row["scoped_folders"], json!([{"name": "@projects", "mode": "read_write"}]));
        assert!(mac_row["stats"][0].as_str().unwrap().starts_with("code: 1/1 ok"), "{}", mac_row["stats"]);
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
        db.upsert_provider(&ProviderConfig {
            id: "gpu-host".into(),
            kind: "scripted".into(),
            base_url: None,
            api_key: None,
            options: json!({"via_device": pc.id, "inline": {"default": []}}),
        })
        .unwrap();
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
            "MacBook lacks gpu (an admin-configured hosted model). Devices that have it: Gaming PC (offline). \
             Name one that has every need, or leave device out."
        );
        assert!(hub.db.threads(Some(&conv)).is_empty(), "a refused start records no thread");

        let err = hub
            .start_thread(
                &conv,
                ThreadRequest { title: "Build".into(), goal: "Compile.".into(), needs: vec!["shell".into()], ..Default::default() },
            )
            .await
            .unwrap_err();
        assert!(err.contains("No online device satisfies shell") && err.contains("Gaming PC (offline)"), "{err}");
        assert!(err.contains("MacBook (offline)") && err.contains("file work"), "{err}");

        let err = hub
            .start_thread(
                &conv,
                ThreadRequest { title: "T".into(), goal: "G".into(), needs: vec!["cuda".into()], ..Default::default() },
            )
            .await
            .unwrap_err();
        assert!(err.contains("unknown need \"cuda\""), "{err}");

        let thread = hub
            .start_thread(&conv, ThreadRequest { title: "Notes".into(), goal: "Write the notes.".into(), ..Default::default() })
            .await
            .unwrap();
        assert_eq!(thread.device_id, "hub");
        let reason = thread.reason.unwrap();
        assert!(
            reason.contains("device Hub: no other online device fits; file-only work runs on the hub"),
            "{reason}"
        );
        assert!(reason.contains("model m:"), "{reason}");
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            while hub.db.thread(&thread.id).is_some_and(|thread| thread.status == "running") {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the hub thread finishes");
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn install_devices_groups_by_install_id_and_reports_eligibility_and_online_state() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let (hub_dev_id, epoch) = db.ensure_hub_device().unwrap();
        assert_eq!(epoch, 1);

        // One Mac install with two rows (admin client + plain node) sharing install_id.
        let mac_install = "mac_install_12345678";
        let mac_admin = db
            .issue_device("Oscar's Mac", "macos", true, Some(crate::db::Install { id: mac_install, kind: "macos" }), None)
            .unwrap();
        let mac_node = db
            .issue_device(
                "Oscar's Mac",
                "macos",
                false,
                Some(crate::db::Install { id: mac_install, kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();

        // An offline Linux host install that previously reported can_host = true.
        let linux_node = db
            .issue_device(
                "Home Server",
                "linux",
                false,
                Some(crate::db::Install { id: "linux_install_123456", kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();

        // An older-version Windows host install (connected).
        let old_win = db
            .issue_device(
                "Old PC",
                "windows",
                false,
                Some(crate::db::Install { id: "win_old_install_1234", kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();

        // An iPhone (cannot host).
        let _iphone = db
            .issue_device(
                "Oscar's iPhone",
                "ios",
                true,
                Some(crate::db::Install { id: "ios_install_12345678", kind: "ios" }),
                None,
            )
            .unwrap();

        let hub = Hub::new(db, dir.clone());
        hub.record_host_info(
            &linux_node.device.id,
            NodeHostInfo { can_host: true, app_version: HUB_VERSION.into(), free_bytes: Some(500_000_000), cloudflared: true },
        );

        // Start a real listener so the Mac node and Old PC can connect with Hello { host }.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(hub.clone());
        let srv = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });

        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let connect_with_host = |token: String, host: NodeHostInfo| async move {
            let mut req = format!("ws://{addr}{}", core_protocol::CONNECT_PATH).into_client_request().unwrap();
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let hello = core_protocol::NodeMessage::Hello {
                protocol_version: core_protocol::PROTOCOL_VERSION,
                client_version: host.app_version.clone(),
                host: Some(host),
            };
            ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();
            while let Some(Ok(WsMsg::Text(text))) = ws.next().await {
                if matches!(serde_json::from_str::<core_protocol::HubMessage>(&text), Ok(core_protocol::HubMessage::Welcome { .. })) {
                    break;
                }
            }
            ws
        };

        let _mac_ws = connect_with_host(
            mac_node.token.clone(),
            NodeHostInfo {
                can_host: true,
                app_version: HUB_VERSION.into(),
                free_bytes: Some(1_000_000_000),
                cloudflared: false,
            },
        )
        .await;
        let _old_win_ws = connect_with_host(
            old_win.token.clone(),
            NodeHostInfo {
                can_host: true,
                app_version: "0.0.1".into(),
                free_bytes: Some(1_000_000_000),
                cloudflared: true,
            },
        )
        .await;

        let http = reqwest::Client::new();
        // Unauthenticated request is rejected.
        let unauth = http.get(format!("http://{addr}/v1/devices/installs")).send().await.unwrap();
        assert_eq!(unauth.status(), reqwest::StatusCode::UNAUTHORIZED);

        let resp: InstallDevicesResponse = http
            .get(format!("http://{addr}/v1/devices/installs"))
            .bearer_auth(&mac_admin.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(resp.hub_device_id, hub_dev_id);
        assert_eq!(resp.epoch, 1);

        // Current hub is first, ticked, not selectable.
        assert!(resp.installs[0].is_hub);
        assert!(!resp.installs[0].selectable);
        assert_eq!(resp.installs[0].reason.as_deref(), Some("current_hub"));

        // Mac install collapses admin + node rows into one entry, eligible + online + selectable, with cloudflared warning.
        let mac = resp.installs.iter().find(|d| d.install_id.as_deref() == Some(mac_install)).unwrap();
        assert_eq!(mac.device_ids.len(), 2);
        assert_eq!(mac.node_device_id.as_deref(), Some(mac_node.device.id.as_str()));
        assert!(mac.online);
        assert!(mac.eligible);
        assert!(mac.selectable);
        assert_eq!(mac.reason, None);
        assert!(mac.warnings.iter().any(|w| w.contains("cloudflared")), "{:?}", mac.warnings);

        // Linux install is eligible but offline.
        let linux = resp.installs.iter().find(|d| d.name == "Home Server").unwrap();
        assert!(linux.eligible);
        assert!(!linux.online);
        assert!(!linux.selectable);
        assert_eq!(linux.reason.as_deref(), Some("offline"));

        // Older Windows install is online + eligible but blocked by older_version.
        let old = resp.installs.iter().find(|d| d.name == "Old PC").unwrap();
        assert!(old.eligible);
        assert!(old.online);
        assert!(!old.selectable);
        assert_eq!(old.reason.as_deref(), Some("older_version"));

        // iPhone is ineligible.
        let phone = resp.installs.iter().find(|d| d.name == "Oscar's iPhone").unwrap();
        assert!(!phone.eligible);
        assert!(!phone.selectable);
        assert_eq!(phone.reason.as_deref(), Some("cannot_host"));

        srv.abort();
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn embedded_hub_and_node_share_one_install_and_hub_is_excluded_from_move_targets() {
        let dir = tmp();
        let db = Db::open(&dir).unwrap();
        let pc_install = "pc_install_0123456789abcdef0123";
        let (hub_dev_id, epoch) = db.ensure_hub_device_for_install(Some(pc_install)).unwrap();
        assert_eq!(epoch, 1);

        // Same PC registers its admin client row and its node row (reusing the hub row).
        let pc_admin = db
            .issue_device("Oscar's PC", "windows", true, Some(crate::db::Install { id: pc_install, kind: "windows" }), None)
            .unwrap();
        let pc_node = db
            .issue_device(
                "Oscar's PC",
                "windows",
                false,
                Some(crate::db::Install { id: pc_install, kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();
        assert_eq!(pc_node.device.id, hub_dev_id, "node on same install reuses hub row");

        // Second computer using legacy `<install_id>-node` for its node row still groups with its admin row.
        let laptop_install = "laptop_install_0123456789abcdef";
        let laptop_legacy_node = format!("{laptop_install}-node");
        let laptop_admin = db
            .issue_device("Laptop", "macos", true, Some(crate::db::Install { id: laptop_install, kind: "macos" }), None)
            .unwrap();
        let laptop_node = db
            .issue_device(
                "Laptop",
                "macos",
                false,
                Some(crate::db::Install { id: &laptop_legacy_node, kind: crate::db::NODE_INSTALL }),
                None,
            )
            .unwrap();

        let hub = Hub::new(db, dir.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = crate::server::router(hub.clone());
        let srv = tokio::spawn(async move { let _ = axum::serve(listener, app).await; });

        use futures_util::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message as WsMsg;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest;

        let connect_host = |token: String| async move {
            let mut req = format!("ws://{addr}{}", core_protocol::CONNECT_PATH).into_client_request().unwrap();
            req.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
            let (mut ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
            let hello = core_protocol::NodeMessage::Hello {
                protocol_version: core_protocol::PROTOCOL_VERSION,
                client_version: HUB_VERSION.into(),
                host: Some(NodeHostInfo {
                    can_host: true,
                    app_version: HUB_VERSION.into(),
                    free_bytes: Some(1_000_000_000),
                    cloudflared: true,
                }),
            };
            ws.send(WsMsg::Text(serde_json::to_string(&hello).unwrap().into())).await.unwrap();
            while let Some(Ok(WsMsg::Text(text))) = ws.next().await {
                if matches!(serde_json::from_str::<core_protocol::HubMessage>(&text), Ok(core_protocol::HubMessage::Welcome { .. })) {
                    break;
                }
            }
            ws
        };

        // Both the hub's own node and the second PC's node are connected and report can_host: true.
        let _pc_ws = connect_host(pc_node.token.clone()).await;
        let _laptop_ws = connect_host(laptop_node.token.clone()).await;

        let http = reqwest::Client::new();
        let resp: InstallDevicesResponse = http
            .get(format!("http://{addr}/v1/devices/installs"))
            .bearer_auth(&pc_admin.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        // Only 2 install entries total: the PC (hub + node + admin) and the Laptop (admin + legacy node).
        assert_eq!(resp.installs.len(), 2, "{:?}", resp.installs);

        let pc_entry = &resp.installs[0];
        assert!(pc_entry.is_hub);
        assert_eq!(pc_entry.install_id.as_deref(), Some(pc_install));
        assert_eq!(pc_entry.device_id, hub_dev_id);
        assert_eq!(pc_entry.node_device_id.as_deref(), Some(hub_dev_id.as_str()));
        assert_eq!(pc_entry.device_ids.len(), 2);
        assert_eq!(pc_entry.rows.len(), 2);
        assert!(pc_entry.rows.iter().any(|r| r.id == hub_dev_id && !r.admin && r.is_hub && r.online));
        assert!(pc_entry.rows.iter().any(|r| r.id == pc_admin.device.id && r.admin && !r.is_hub));
        assert!(!pc_entry.selectable);
        assert_eq!(pc_entry.reason.as_deref(), Some("current_hub"));

        let laptop_entry = &resp.installs[1];
        assert!(!laptop_entry.is_hub);
        assert_eq!(laptop_entry.install_id.as_deref(), Some(laptop_install));
        assert_eq!(laptop_entry.device_ids.len(), 2);
        assert!(laptop_entry.device_ids.contains(&laptop_admin.device.id));
        assert!(laptop_entry.device_ids.contains(&laptop_node.device.id));
        assert_eq!(laptop_entry.rows.len(), 2);
        assert!(laptop_entry.selectable);

        // Move targets list excludes the hub's own install and offers only the other host install.
        let targets = resp.move_targets();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].install_id.as_deref(), Some(laptop_install));
        assert!(targets.iter().all(|t| !t.is_hub && t.install_id.as_deref() != Some(pc_install)));

        // Existing per-row /v1/devices endpoint continues working.
        let raw_devices: Value = http
            .get(format!("http://{addr}/v1/devices"))
            .bearer_auth(&pc_admin.token)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert!(raw_devices.as_array().unwrap().len() >= 4);

        srv.abort();
        drop(hub);
        std::fs::remove_dir_all(dir).ok();
    }
}
