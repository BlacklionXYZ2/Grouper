//! Device selection hub glue: builds [`DeviceProfile`]s from live presence and the
//! tool/folder registry, then delegates ranking and need matching to [`core_registry::devices`].

use core_protocol::{FILE_TOOLS, Permission};
pub use core_registry::devices::{DevicePick, DeviceSuggestion, RankedDevice};
use core_registry::devices::{DeviceProfile, Need, lack_error, missing_needs, parse_needs, satisfied_tokens, suggest_from_conn};
use serde_json::{Value, json};

use crate::registry::{By, stats_line};
use crate::server::{HUB_DEVICE, Hub};

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
            "MacBook lacks gpu (a hosted local model or a tool whose name contains \"gpu\"). Devices that have it: Gaming PC (offline). \
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
}
