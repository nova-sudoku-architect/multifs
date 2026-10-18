# AGENTS.md — MultiFS Project Rules

Rules for any AI agent (and human) working on this repo. Treat these as constraints, not suggestions.

## Never commit deployment configs — non-negotiable

**Deployment/instance configuration never goes in this repo.** A deployment config
describes one specific host (real account emails, bind addresses, TLS domains,
per-account quotas, local paths). It is environment-specific and often sensitive,
so it must stay on the host that uses it (e.g. `/etc/multifs.toml`).

- Do **not** add, restore, or commit a `config.deploy.toml` (or any real instance
  config). It is git-ignored for a reason — leave it that way.
- The only config artifact in the repo is [`config.example.toml`](config.example.toml):
  a **generic, commented template** with placeholder values. It is a schema
  reference, **not** a copy of any deployment.
- You may consult a live config to understand the schema, then hand-write a
  generic example from it. Do not paste it, real account names, quotas, IPs, or
  domains into the repo.
- Secrets are a separate rule (see [Secrets](#secrets)): env vars only.

## Keep docs in sync with code — non-negotiable

Whenever you change code, update the docs in the **same commit** (or a follow-up commit before the change is considered done). A change that alters behavior, CLI, config, or data model without a matching doc update is **incomplete**.

### Docs that must stay accurate

| File | What it covers | Update when you change… |
|------|----------------|--------------------------|
| `README.md` | Landing page: intro, features table, quick start, docs index | features, the docs map |
| `docs/user-guide.md` | Concepts, S3 usage, web UI, folder metadata, symlinks | user-facing behavior, terminology, web UI |
| `docs/operator-guide.md` | Configuration, backends, deploy, observability, GC | config, placement, deploy, GC, metrics |
| `docs/cli-reference.md` | Full CLI reference | any command/subcommand/flag |
| `docs/planning.md` | Design goals, **caveats & limitations**, roadmap | limitations, roadmap / future work |
| `docs/developer-guide.md` | Repo layout, data-flow diagrams, tests, adding a backend | module layout, request flows, build/test |
| `docs/architecture.md` | Data model, MVCC read/write/delete paths, multipart, GC, placement, backend interface, known issues, **test count** | schema, engine logic, storage backends, known issues, `cargo test --lib` count |
| `config.example.toml` | Annotated **generic** config template | `src/config.rs` fields (add/remove/rename, defaults) |

> The README was split into audience pages (issue #13): read the landing README
> for orientation, then the page for your audience. A behavior change usually
> touches **operator-guide + cli-reference** (if CLI/config), **user-guide** (if
> user-visible), and **architecture.md** (always, incl. the test count).

### Checklist before committing a behavior change

- [ ] README features / docs map updated if the feature set changed
- [ ] the relevant audience page(s) updated — user-guide / operator-guide / cli-reference / planning
- [ ] `docs/architecture.md` updated — **including the test count**, which must match `cargo test --lib`
- [ ] config examples updated if `src/config.rs` changed (generic template only — no deployment values)
- [ ] No stale claims (grep for removed features: `WebDAV`, `chunking`, `32 MB`, `erasure`, `page cache`)

### Rule of thumb

If a reviewer can't tell what the code does by reading the docs, the change isn't done.

## Issues & PRs — every enhancement is tracked — non-negotiable

Any **enhancement** (new feature, behavior change, refactor, perf work) must be
tracked as a **GitHub issue** and land via a **pull request** into `main` — never a
direct push. The issue/PR is the durable record of *what* changed and *why*, so a
future reader can see the reasoning and what was evaluated, not just the diff.

### Issue first

Open an issue describing the enhancement **before** implementing. It must capture:

- **Problem / motivation** — what hurts today, with observed evidence (numbers,
  failing case, user report).
- **Proposed change** — the intended approach.
- **Options evaluated** — alternatives considered (other algorithms, existing
  libraries/plugins/services per the preflight rule) and why the chosen one wins,
  including trade-offs and rejected options.
- **Acceptance criteria** — definition of done.

### Then a PR

1. Branch off `main`: `feat/…`, `perf/…`, `refactor/…`, `fix/…`, or `docs/…` (one
   branch per issue).
2. Commit with a conventional-commit message; keep docs in sync (above).
3. **Review the docs for alignment** — mandatory, on every PR (see
   [docs review gate](#docs-review-gate--mandatory-on-every-pr)). Walk the
   README landing page **and the relevant audience page(s)** against your change
   and fix anything they now contradict.
4. Open a PR that links the issue (`Closes #N` where it completes it). The PR body
   must state:
   - **what changed and why**,
   - **what was evaluated** — alternatives considered and why rejected (design,
     perf, security trade-offs),
   - **verification evidence** — `cargo test --lib` count, `cargo clippy`,
     and any manual/live check,
   - **docs review** — the explicit statement required by the gate below
     (reviewed ✅ + what changed, or N/A + why no doc needs a change),
   - **docs updated** + any CLI / config / data-model impact.
   - For security-sensitive work (auth, quotas, exposure), document the threat
     model and the safe-default decision.
5. Merge (`--no-ff` merge or squash) keeping the issue reference so history is
   traceable, then delete the branch. **A PR is not ready to merge until the
   docs review gate is satisfied.**

### Docs review gate — mandatory on every PR

The docs are the front door, split by audience: `README.md` (landing: features
+ docs map), `docs/user-guide.md`, `docs/operator-guide.md`,
`docs/cli-reference.md`, `docs/planning.md`, `docs/developer-guide.md`, and
`docs/architecture.md`. A change that alters behavior, the CLI, config, the data
model, performance characteristics, or the deployment surface can silently
contradict one of them. **Every PR must explicitly review the affected doc(s)
and confirm alignment — no exceptions.**

Before requesting merge, walk the docs against the diff and confirm each that applies:

- [ ] **Features table** (`README.md`) — any new/removed/changed capability is reflected.
- [ ] **CLI reference** (`docs/cli-reference.md`) — every added/changed/removed command,
      subcommand, flag, or alias appears, with accurate usage.
- [ ] **Config** (`docs/operator-guide.md` + `config.example.toml`) — any config field
      added/renamed/removed is reflected in both.
- [ ] **User guide / operator guide** — user-visible behavior and run/operate steps match.
- [ ] **Planning** (`docs/planning.md`) — caveats/limitations and roadmap stay truthful
      (move shipped items to *Recently shipped*, don't leave a done item "planned").
- [ ] **Developer guide / architecture** — module layout, request flows, and diagrams match;
      **test count** matches `cargo test --lib`.
- [ ] **Diagrams & prose** — sequence/flow diagrams still match the code; no stale claims
      about removed features (`WebDAV`, `chunking`, `32 MB`, `erasure`, `page cache`, `shard`, …).
- [ ] **Links** — no dangling `#anchors` (pages were split; cross-links must point at the
      right page).
- [ ] **Numbers** — any quoted count/number (accounts, test count, ports, limits) is still correct.

The PR body must contain a **docs review** line, one of:

- `docs reviewed — updated: <which page(s), what changed>` , or
- `docs reviewed — no change needed: <why they stay accurate>` .

If no doc genuinely needs a change, say so explicitly (and why) — an absent doc
mention is a blocking gap, not an implicit "N/A".

**Exempt:** pure typo/comment fixes and doc-only nits may go straight to `main`.
When in doubt, open an issue.

Every issue/PR must be **self-contained**: someone reading only it should understand
the change and its rationale without reading the code.

### Pre-push review hook — automated, but not a substitute

`.githooks/` holds a client-side hook that runs before a push. It sends the
diff plus this repo's own authority files (`README.md` docs map/features,
`AGENTS.md` deployment-config + docs-sync + issues/PRs + docs review gate) to an
LLM and blocks on a `fail` verdict. Axes: terminology, doc alignment,
deployment-config & secret safety, architecture & correctness.

```bash
bash .githooks/install.sh            # enable for this clone (core.hooksPath)
git config --unset core.hooksPath    # disable
python3 .githooks/ai_review.py --range main..HEAD --no-block   # dry run
python3 .githooks/ai_review.py --print-conventions            # see the authority
AI_REVIEW_SKIP=1 git push            # skip the review
```

Requires `pip install openai` and an API key (`DEEPSEEK_API_KEY` or
`DEEPSEEK_CODING_API_KEY`, from the environment or `~/.openclaw/.env`). With no
key it skips (fail-open) rather than blocking.

#### Two triggers — the PR one cannot be missed

| Trigger | Can be bypassed | Comments on the PR |
|---|---|---|
| Pre-push hook (local) | yes — `--no-verify`, `AI_REVIEW_SKIP=1` | no |
| GitHub workflow (`.github/workflows/ai-review.yml`) | only by not opening a PR | **yes** |

The workflow runs on `pull_request: opened / synchronize / reopened`, so the
review is attached to the **PR**, not to a push. That matters: a client-side hook
misses a branch's first push if the guard is wrong, and misses everything for a
contributor who never ran `install.sh`.

#### Incremental reviews

The posted comment carries a hidden state block with the reviewed commit:

```html
<!-- ai-review-state: {"sha":"<40-hex>","verdict":"pass",...} -->
```

The next run reviews only `previous_sha..head` and feeds the earlier findings
back to the reviewer, so fixed issues are not re-reported and unchanged code is
not re-analysed. It falls back to a **full** review when there is no earlier
comment, when disabled (`--no-incremental`), or when the previous commit is no
longer an ancestor of the head (force-push — a rewrite can reintroduce anything).

```bash
git fetch origin main
python3 .githooks/ai_review.py --range origin/main...HEAD \
  --repo OWNER/REPO --pr 12 --post-comment
```

A triple-dot range is resolved through `git merge-base`.

The engine (`.githooks/ai_review.py`) is **portable** — the same file is used in
sibling repos. Everything repo-specific lives in `.githooks/review.json`
(reviewed paths, authority files, axes). **Edit the config, never fork the
engine**; two copies of logic drift, two copies of config cannot.

What it does **not** do: it cannot verify that an issue exists, it cannot read
the PR body, and it can be bypassed with `--no-verify`. The **docs review gate
above stays mandatory** — the hook catches stale docs in the diff, but the
`docs reviewed — …` line in the PR body is still yours to write.

A review can also be wrong. When it is, say so **on the PR, with evidence** —
a false HIGH teaches reviewers to ignore the hook, which is worse than no review.

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
