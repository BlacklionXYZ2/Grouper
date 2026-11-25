//! Model and device registry learning, routing, cost estimation and deterministic probes.
//!
//! Pure, portable logic over plain data and a [`rusqlite::Connection`], reusable
//! by the hub and by clients (including phones) without HTTP, async runtime, or
//! provider dependencies.

pub mod cost;
pub mod devices;
pub mod probes;
pub mod registry;

pub use core_protocol;
pub use registry::{
    By, MAX_USE_CASES_CHARS, MIN_ROUTING_RUNS, OutcomeRow, PROPOSAL_HIGH, PROPOSAL_LOW, PROPOSAL_MIN_RUNS, Route,
    SCHEMA, TASK_TYPES, add_rule, check_use_cases, clean_tags, combine_use_cases, default_model_from, delete_rule,
    decide_proposal, find_model_in, format_duration_s, grade_outcome, is_local_url, local_map, migrate, model_is_local,
    models_overview, norm, outcome_key, outcome_stats, outcome_stats_by_source, probe_overview_line, probe_stats,
    proposal, proposals, propose_use_cases, rate_outcome, record_probe_outcome, record_thread_outcome, registry_stats,
    route_model, route_model_from, rules, set_rule_enabled, set_thread_meta, stats_line, strip_probe_clause, summarize,
    thread_meta, upsert_probe_proposal, validate_grade, validate_rule,
};

pub(crate) fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
