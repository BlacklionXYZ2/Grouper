# Instructions for agents working on Grouper

- **Log every change in `Changelogs/`.** Before you finish, append an entry to
  `Changelogs/YYYY-MM-DD.md` (today's date) saying what you changed and why, in the
  format in `Changelogs/README.md`. Include what you verified and what's left open.
- Design: `Grouper — Proposal v3.docx`. Build and run: `README.md`.
- Rust: `cargo build --workspace`, `cargo test --workspace`, `cargo clippy --workspace`.
- After changing `crates/ffi`, run `scripts/build-apple.sh` to regenerate the Swift bindings.
