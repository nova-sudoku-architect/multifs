# AGENTS.md — MultiFS Project Rules

Rules for any AI agent (and human) working on this repo. Treat these as constraints, not suggestions.

## Keep docs in sync with code — non-negotiable

Whenever you change code, update the docs in the **same commit** (or a follow-up commit before the change is considered done). A change that alters behavior, CLI, config, or data model without a matching doc update is **incomplete**.

### Docs that must stay accurate

| File | What it covers | Update when you change… |
|------|----------------|--------------------------|
| `README.md` | Features table, CLI reference, config, GC section | features, CLI commands/flags, placement, vacuum/import behavior |
| `docs/architecture.md` | Data model, MVCC read/write/delete paths, multipart, GC, placement, backend interface, known issues, **test count** | schema, engine logic, storage backends, known issues, `cargo test --lib` count |
| `config.example.toml` | Annotated config schema | `src/config.rs` fields (add/remove/rename, defaults) |
| `config.deploy.toml` | Deployed-config mirror | when the live `/etc/multifs.toml` changes |

### Checklist before committing a behavior change

- [ ] `README.md` features / CLI / config updated
- [ ] `docs/architecture.md` updated — **including the test count**, which must match `cargo test --lib`
- [ ] config examples updated if `src/config.rs` changed
- [ ] No stale claims (grep for removed features: `WebDAV`, `chunking`, `32 MB`, `erasure`, `page cache`)

### Rule of thumb

If a reviewer can't tell what the code does by reading the docs, the change isn't done.

## Secrets

Passwords, API keys, and OAuth tokens live in env vars (`~/.openclaw/.env` / `/etc/multifs.env`), never in code, config, or docs. Config files reference them by env-var name only (`token_env = "PCLOUD_TOKEN_VIDEO_01"`).

## Token savings — use rtk

Prefix verbose shell commands with `rtk` to cut token usage 50–90%: `rtk git
status/diff/log`, `rtk ls`, `rtk read <file>`, `rtk grep "pattern"`, `rtk find`,
`rtk cargo test/build/clippy`, `rtk docker ps/logs`, `rtk kubectl`. Rule: any
command likely to emit >100 lines gets the `rtk` prefix. Skip interactive
commands, already-minimal output, and pipes/heredocs (rtk auto-skips). On
failure read the tee log for full output. rtk is an *output display layer* only —
never hide a mutating command's result (commit, push, deploy).

## Coding tasks — use codex + rtk

Feature builds, refactors, and non-trivial code changes in this repo are
delegated to **Codex** as a background worker, with build/test/clippy output
compressed via **rtk**.

- Run the agent with `codex exec` (background, `pty`), not in the foreground
  OpenClaw session.
- Codex is wired directly to **DeepSeek** (see `~/.codex/config.toml`;
  authorization via `DEEPSEEK_CODING_API_KEY` in `~/.openclaw/.env`).
- **rtk** compresses verbose command output inside the worker: prefix build /
  test / clippy / git commands (`rtk cargo build --release`, `rtk cargo test
  --lib`, `rtk cargo clippy`, `rtk git status`).
- On this host, Codex's bubblewrap sandbox can't create network namespaces
  (`bwrap: loopback: Failed RTM_NEWADDR`). The host is already externally
  sandboxed, so launch with
  `--dangerously-bypass-approvals-and-sandbox` (equivalent to
  `sandbox: danger-full-access`); a sandboxed launch will fail every command.
- Don't schedule heavy Codex work during DeepSeek **peak hours** — see below.

### MultiFS-specific build/test

- Build: `rtk cargo build --release`
- Tests: `rtk cargo test --lib` — final passing count must be synced into
  `docs/architecture.md` (non-negotiable).
- Clippy: `rtk cargo clippy`

## Subagent model routing — use Flash for simple tasks

When delegating via `sessions_spawn`, do NOT default the child to the current
model. Most delegated work is mechanical → run it on the cheaper, faster
**deepseek/deepseek-v4-flash**.

- **Simple / mechanical → flash:** summarize, translate, list/enumerate,
  reformat, quick lookup, batch repetitive work, data extraction, file/grep/read.
- **Complex / reasoning → main agent (deepseek-v4-pro):** architecture,
  multi-step debugging, trade-offs, synthesis, judgment calls.

How: `sessions_spawn(..., model="deepseek/deepseek-v4-flash")` with a
self-contained brief, `mode="run"` for one-shot work, omit `context` unless the
child needs the transcript. Question: "reasoning or execution?" Execution →
flash. Reasoning → keep on Pro.

## DeepSeek peak hours — remind the user

Peak hours (rates ×2): **01:00–04:00 and 06:00–10:00 UTC, Mon–Fri** (that's
09:00–12:00 and 14:00–18:00 in Asia/Hong_Kong, UTC+8). All other hours are
off-peak (half price).

If the user asks you to do anything during peak hours, **remind them** that
DeepSeek peak rates are 2× off-peak and suggest deferring non-urgent work to
off-peak hours.
