# Changelogs

Every change any agent (Claude Code, Codex, Antigravity, Hermes, …) or person makes
to this project is logged here, with **why** it was made.

## Rules

- One file per day: `YYYY-MM-DD.md`. Append to today's file; create it if missing.
- One entry per change set (a task, not every keystroke). Newest entry at the bottom.
- Log before you finish: an agent's work isn't done until its entry is written.
- Record what you actually did, including things you tried and reverted, and anything
  left unfinished or unverified.
- Don't edit other entries except to fix a typo; add a new entry to correct or undo one.

## Entry format

```markdown
## HH:MM — Short title

- **Agent:** who made the change (tool and model, e.g. Claude Code · claude-opus-5-5)
- **Asked by:** the request that led to it, in a sentence
- **Changed:**
  - `path/to/file` — what changed
- **Why:** the reasoning: the problem, the decision, alternatives rejected
- **Verified:** what was run or checked, and the result
- **Open:** anything left undone, untested or needing a decision (or "nothing")
```
