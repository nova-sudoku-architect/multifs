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
