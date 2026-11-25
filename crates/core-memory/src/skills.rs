//! Skills: procedures that worked, saved by the orchestrator after a successful
//! multi-step task (in the style of Hermes): the steps, the tools used, and which
//! models and devices handled them. Similar tasks reuse a skill instead of
//! re-planning.
//!
//! Stored in `skills` with an FTS5 index (`skills_fts`) kept in sync by triggers,
//! like memory. Each project-chat turn, [`skills_block`] searches them with the
//! turn's text and appends the best [`SKILLS_TOP`] as a `[Skills]` block after the
//! `[Memory]` block, skipping skills already shown in the conversation.

use anyhow::{Result, bail};
use core_protocol::{Skill, SkillInput};
use rusqlite::{Connection, OptionalExtension, params};

use crate::memory::fts_query;
use crate::{clip, now};

pub const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS skills (
  id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, description TEXT NOT NULL,
  triggers TEXT NOT NULL DEFAULT '', steps TEXT NOT NULL, tools TEXT NOT NULL DEFAULT '',
  models TEXT NOT NULL DEFAULT '', devices TEXT NOT NULL DEFAULT '', success_count INTEGER NOT NULL DEFAULT 0,
  conversation_id TEXT, last_used INTEGER, created INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE UNIQUE INDEX IF NOT EXISTS skills_name ON skills (name COLLATE NOCASE);
CREATE VIRTUAL TABLE IF NOT EXISTS skills_fts USING fts5(
  name, description, triggers, steps, content='skills', content_rowid='id',
  tokenize='porter unicode61 remove_diacritics 2');
CREATE TRIGGER IF NOT EXISTS skills_ai AFTER INSERT ON skills BEGIN
  INSERT INTO skills_fts (rowid, name, description, triggers, steps)
  VALUES (new.id, new.name, new.description, new.triggers, new.steps);
END;
CREATE TRIGGER IF NOT EXISTS skills_ad AFTER DELETE ON skills BEGIN
  INSERT INTO skills_fts (skills_fts, rowid, name, description, triggers, steps)
  VALUES ('delete', old.id, old.name, old.description, old.triggers, old.steps);
END;
CREATE TRIGGER IF NOT EXISTS skills_au AFTER UPDATE OF name, description, triggers, steps ON skills BEGIN
  INSERT INTO skills_fts (skills_fts, rowid, name, description, triggers, steps)
  VALUES ('delete', old.id, old.name, old.description, old.triggers, old.steps);
  INSERT INTO skills_fts (rowid, name, description, triggers, steps)
  VALUES (new.id, new.name, new.description, new.triggers, new.steps);
END;
";

/// Create the `skills` table, FTS5 virtual table and sync triggers.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

const MAX_NAME: usize = 80;
const MAX_DESCRIPTION: usize = 500;
const MAX_STEPS: usize = 8000;
const MAX_LIST: usize = 30;
/// Skills put in front of the orchestrator each project turn, and how much of their steps.
pub const SKILLS_TOP: usize = 2;
const STEPS_IN_BLOCK: usize = 700;

const COLS: &str = "s.id, s.name, s.description, s.triggers, s.steps, s.tools, s.models, s.devices, s.success_count, \
                    s.last_used, s.created, s.updated";

pub fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(str::trim).filter(|t| !t.is_empty()).map(String::from).collect()
}

/// Trimmed, de-duplicated (ignoring case) and capped; commas inside an item split it.
fn clean_list(items: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in items.iter().flat_map(|t| t.split(',')).map(str::trim).filter(|t| !t.is_empty()) {
        if !out.iter().any(|o| o.eq_ignore_ascii_case(t)) && out.len() < MAX_LIST {
            out.push(t.to_string());
        }
    }
    out
}

fn merge(old: &[String], new: &[String]) -> Vec<String> {
    clean_list(&old.iter().chain(new).cloned().collect::<Vec<_>>())
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<Skill> {
    Ok(Skill {
        id: r.get(0)?,
        name: r.get(1)?,
        description: r.get(2)?,
        triggers: split_list(&r.get::<_, String>(3)?),
        steps: r.get(4)?,
        tools: split_list(&r.get::<_, String>(5)?),
        models: split_list(&r.get::<_, String>(6)?),
        devices: split_list(&r.get::<_, String>(7)?),
        success_count: r.get::<_, i64>(8)? as u64,
        last_used: r.get::<_, Option<i64>>(9)?.map(|v| v as u64),
        created: r.get::<_, i64>(10)? as u64,
        updated: r.get::<_, i64>(11)? as u64,
    })
}

fn text_field(value: &str, what: &str, max: usize) -> Result<String> {
    let v = value.trim();
    if v.is_empty() {
        bail!("{what} is empty");
    }
    if v.chars().count() > max {
        bail!("{what} is longer than {max} characters");
    }
    Ok(v.to_string())
}

fn get_by_name(conn: &Connection, name: &str) -> Option<Skill> {
    conn.query_row(&format!("SELECT {COLS} FROM skills s WHERE s.name = ?1 COLLATE NOCASE"), [name.trim()], row)
        .optional()
        .ok()
        .flatten()
}

/// Save a skill after a successful task. A skill with the same name (ignoring
/// case) is updated instead: new description and steps, lists merged, and one
/// more success. Returns the skill and whether it was new.
pub fn save(conn: &Connection, input: &SkillInput, conversation_id: Option<&str>) -> Result<(Skill, bool)> {
    let name = text_field(input.name.as_deref().unwrap_or_default(), "name", MAX_NAME)?;
    let description = text_field(input.description.as_deref().unwrap_or_default(), "description", MAX_DESCRIPTION)?;
    let steps = text_field(input.steps.as_deref().unwrap_or_default(), "steps", MAX_STEPS)?;
    let list = |l: &Option<Vec<String>>| clean_list(l.as_deref().unwrap_or_default());
    let existing = get_by_name(conn, &name);
    let t = now() as i64;
    let id = match &existing {
        Some(s) => {
            conn.execute(
                "UPDATE skills SET description = ?1, steps = ?2, triggers = ?3, tools = ?4, models = ?5,
                   devices = ?6, success_count = success_count + 1, updated = ?7 WHERE id = ?8",
                params![
                    description,
                    steps,
                    merge(&s.triggers, &list(&input.triggers)).join(","),
                    merge(&s.tools, &list(&input.tools)).join(","),
                    merge(&s.models, &list(&input.models)).join(","),
                    merge(&s.devices, &list(&input.devices)).join(","),
                    t,
                    s.id
                ],
            )?;
            s.id
        }
        None => {
            conn.execute(
                "INSERT INTO skills (name, description, triggers, steps, tools, models, devices, success_count,
                   conversation_id, created, updated) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1, ?8, ?9, ?9)",
                params![
                    name,
                    description,
                    list(&input.triggers).join(","),
                    steps,
                    list(&input.tools).join(","),
                    list(&input.models).join(","),
                    list(&input.devices).join(","),
                    conversation_id,
                    t
                ],
            )?;
            conn.last_insert_rowid()
        }
    };
    Ok((get(conn, id).expect("just saved"), existing.is_none()))
}

pub fn get(conn: &Connection, id: i64) -> Option<Skill> {
    conn.query_row(&format!("SELECT {COLS} FROM skills s WHERE s.id = ?1"), [id], row).optional().ok().flatten()
}

/// Change a skill. Given lists replace the old ones, or with `merge_lists` are
/// added to them; `worked` counts one more success. `None` if there's no such skill.
pub fn update(conn: &Connection, id: i64, input: &SkillInput, merge_lists: bool, worked: bool) -> Result<Option<Skill>> {
    let Some(s) = get(conn, id) else { return Ok(None) };
    let name = match &input.name {
        Some(n) => {
            let n = text_field(n, "name", MAX_NAME)?;
            if get_by_name(conn, &n).is_some_and(|o| o.id != id) {
                bail!("another skill is already called {n:?}");
            }
            n
        }
        None => s.name.clone(),
    };
    let description = match &input.description {
        Some(d) => text_field(d, "description", MAX_DESCRIPTION)?,
        None => s.description.clone(),
    };
    let steps = match &input.steps {
        Some(d) => text_field(d, "steps", MAX_STEPS)?,
        None => s.steps.clone(),
    };
    let list = |old: &[String], new: &Option<Vec<String>>| match new {
        Some(n) if merge_lists => merge(old, n),
        Some(n) => clean_list(n),
        None => old.to_vec(),
    };
    conn.execute(
        "UPDATE skills SET name = ?1, description = ?2, steps = ?3, triggers = ?4, tools = ?5, models = ?6,
           devices = ?7, success_count = success_count + ?8, updated = ?9, last_used = CASE WHEN ?8 > 0 THEN ?9
           ELSE last_used END WHERE id = ?10",
        params![
            name,
            description,
            steps,
            list(&s.triggers, &input.triggers).join(","),
            list(&s.tools, &input.tools).join(","),
            list(&s.models, &input.models).join(","),
            list(&s.devices, &input.devices).join(","),
            worked as i64,
            now() as i64,
            id
        ],
    )?;
    Ok(get(conn, id))
}

pub fn delete(conn: &Connection, id: i64) -> Result<bool> {
    Ok(conn.execute("DELETE FROM skills WHERE id = ?1", [id])? > 0)
}

/// Most recently changed first.
pub fn list(conn: &Connection, limit: usize) -> Vec<Skill> {
    let mut st = conn.prepare(&format!("SELECT {COLS} FROM skills s ORDER BY s.updated DESC, s.id DESC LIMIT ?1")).unwrap();
    st.query_map([limit as i64], row).unwrap().filter_map(Result::ok).collect()
}

/// Best matches for free text, by BM25 (then by successes). Empty when nothing
/// in the text is searchable.
pub fn search(conn: &Connection, text: &str, limit: usize) -> Vec<Skill> {
    let Some(q) = fts_query(text) else { return vec![] };
    let mut st = conn
        .prepare(&format!(
            "SELECT {COLS} FROM skills_fts JOIN skills s ON s.id = skills_fts.rowid
             WHERE skills_fts MATCH ?1 ORDER BY bm25(skills_fts, 4.0, 2.0, 3.0, 1.0), s.success_count DESC LIMIT ?2"
        ))
        .unwrap();
    match st.query_map(params![q, limit as i64], row) {
        Ok(rows) => rows.filter_map(Result::ok).collect(),
        Err(e) => {
            tracing::warn!("skill search failed: {e}");
            vec![]
        }
    }
}

/// Record that these skills were just shown to the orchestrator.
pub fn touch(conn: &Connection, ids: &[i64]) {
    let t = now() as i64;
    for id in ids {
        let _ = conn.execute("UPDATE skills SET last_used = ?1 WHERE id = ?2", params![t, id]);
    }
}

/// How a skill starts in a `[Skills]` block; used to spot skills already shown.
fn marker(s: &Skill) -> String {
    format!("Skill #{} \"", s.id)
}

fn skill_entry(s: &Skill) -> String {
    let mut used = vec![format!("worked {}×", s.success_count)];
    for (what, list) in [("tools", &s.tools), ("models", &s.models), ("devices", &s.devices)] {
        if !list.is_empty() {
            used.push(format!("{what}: {}", list.join(", ")));
        }
    }
    let steps = clip(&s.steps.split_whitespace().collect::<Vec<_>>().join(" "), STEPS_IN_BLOCK);
    format!("{}{}\" ({}): {}\n  Steps: {steps}", marker(s), s.name, used.join("; "), s.description.replace('\n', " "))
}

/// Extract `[Skills]` blocks already present in user message texts, for [`skills_block`]'s `seen`.
pub fn seen_text<'a>(user_texts: impl IntoIterator<Item = &'a str>) -> String {
    user_texts.into_iter().filter(|text| text.contains("[Skills]")).collect::<Vec<_>>().join("\n")
}

/// The `[Skills]` block for a project turn: up to [`SKILLS_TOP`] skills matching
/// `query` that aren't already in `seen` (the conversation so far). Marks them
/// used. `None` when nothing new matches.
pub fn skills_block(conn: &Connection, query: &str, seen: &str) -> Option<String> {
    let found: Vec<Skill> =
        search(conn, query, SKILLS_TOP * 3).into_iter().filter(|s| !seen.contains(&marker(s))).take(SKILLS_TOP).collect();
    if found.is_empty() {
        return None;
    }
    touch(conn, &found.iter().map(|s| s.id).collect::<Vec<_>>());
    let mut out = String::from(
        "[Skills] Saved procedures that worked for similar tasks (reuse one instead of re-planning if it fits; \
         find_skills has the full steps):",
    );
    for s in &found {
        out.push('\n');
        out.push_str(&skill_entry(s));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn
    }

    fn v(t: &[&str]) -> Option<Vec<String>> {
        Some(t.iter().map(|s| s.to_string()).collect())
    }

    fn input(name: &str, description: &str, steps: &str) -> SkillInput {
        SkillInput {
            name: Some(name.into()),
            description: Some(description.into()),
            steps: Some(steps.into()),
            ..Default::default()
        }
    }

    #[test]
    fn save_search_update_delete() {
        let conn = skill_db();
        let mut blog = input("Publish a blog post", "Draft, review and publish a post to the static site.", "1. Draft on hub\n2. Build on mac");
        blog.tools = v(&["write_file", "run_shell", "write_file"]);
        blog.models = v(&["opus"]);
        blog.devices = v(&["hub, mac"]);
        blog.triggers = v(&["blog", "Hugo"]);
        let (a, new) = save(&conn, &blog, Some("c1")).unwrap();
        assert!(new);
        assert_eq!((a.success_count, a.tools.len(), a.devices.clone()), (1, 2, vec!["hub".to_string(), "mac".into()]));
        let (b, _) = save(&conn, &input("Rotate API keys", "Replace provider keys everywhere.", "1. keys set"), None).unwrap();
        assert!(save(&conn, &input("", "d", "s"), None).is_err());
        assert!(save(&conn, &input("x", "d", ""), None).is_err());

        // Same name (any case) updates it: lists merge, one more success.
        let mut again = input("publish a BLOG post", "Draft and publish a post.", "1. Draft\n2. Publish");
        again.models = v(&["sonnet", "OPUS"]);
        let (a2, new) = save(&conn, &again, None).unwrap();
        assert!(!new);
        assert_eq!(a2.id, a.id);
        assert_eq!((a2.success_count, a2.models.clone()), (2, vec!["opus".to_string(), "sonnet".into()]));
        assert_eq!(a2.steps, "1. Draft\n2. Publish");

        // Search: stemming, name and trigger weight, hostile input.
        assert_eq!(search(&conn, "publishing posts", 5)[0].id, a.id);
        assert_eq!(search(&conn, "hugo", 5)[0].id, a.id);
        assert_eq!(search(&conn, "rotate \" OR * NEAR( key:", 5)[0].id, b.id);
        assert!(search(&conn, "the and of", 5).is_empty());

        // Update: merge (tool) or replace (API); worked counts a success.
        let u = update(&conn, a.id, &SkillInput { devices: v(&["pc"]), ..Default::default() }, true, true).unwrap().unwrap();
        assert_eq!((u.success_count, u.devices.len()), (3, 3));
        assert!(u.last_used.is_some());
        let u = update(&conn, a.id, &SkillInput { devices: v(&["pc"]), ..Default::default() }, false, false).unwrap().unwrap();
        assert_eq!((u.success_count, u.devices.clone()), (3, vec!["pc".to_string()]));
        let u = update(&conn, a.id, &SkillInput { steps: Some("1. Use Zola".into()), ..Default::default() }, false, false);
        assert!(u.unwrap().is_some());
        assert_eq!(search(&conn, "zola", 5)[0].id, a.id);
        assert!(update(&conn, a.id, &SkillInput { name: Some("Rotate API keys".into()), ..Default::default() }, false, false).is_err());
        assert!(update(&conn, 999, &SkillInput::default(), false, false).unwrap().is_none());

        assert!(delete(&conn, b.id).unwrap());
        assert!(!delete(&conn, b.id).unwrap());
        assert!(search(&conn, "rotate", 5).is_empty());
        assert_eq!(list(&conn, 10).len(), 1);
    }

    #[test]
    fn skills_blocks() {
        let conn = skill_db();
        assert!(skills_block(&conn, "publish my post", "").is_none());
        for (n, d) in [
            ("Publish a blog post", "Publish a post to the blog."),
            ("Blog post images", "Resize images for a blog post."),
            ("Blog post tags", "Tag a blog post."),
            ("Back up photos", "Copy photos to the NAS."),
        ] {
            let mut i = input(n, d, &"Step detail. ".repeat(100));
            i.models = v(&["opus"]);
            save(&conn, &i, None).unwrap();
        }
        assert!(skills_block(&conn, "Tell me a joke", "").is_none());
        let block = skills_block(&conn, "Publish my blog post please", "").unwrap();
        assert!(block.starts_with("[Skills]"));
        assert_eq!(block.matches("Skill #").count(), SKILLS_TOP);
        assert!(block.contains("Skill #1 \"Publish a blog post\" (worked 1×; models: opus): Publish a post to the blog."), "{block}");
        assert!(block.len() < 2 * STEPS_IN_BLOCK + 600, "{}", block.len());
        assert!(get(&conn, 1).unwrap().last_used.is_some());
        // Shown already: the next best are offered instead, then nothing.
        let next = skills_block(&conn, "blog post", &block).unwrap();
        assert_eq!(next.matches("Skill #").count(), 1);
        assert!(!next.contains("Skill #1 \""));
        assert!(skills_block(&conn, "blog post", &format!("{block}\n{next}")).is_none());
    }
}
