#!/usr/bin/env python3
"""ai_review.py — review a change set against this repo's own docs.

WHY THIS EXISTS
---------------
A repository's rules live in markdown, and code drifts away from them silently.
A stale doc makes the next reader — human or agent — do the wrong thing. So a
reviewer compares the diff against the repo's authority files and returns a
verdict. A `fail` verdict blocks.

TWO TRIGGERS
------------
1. **Pre-push (local).** Runs before `git push` sends anything. Fast feedback,
   but client-side: bypassable, and it cannot comment on a PR.
2. **Pull request (CI).** The workflow runs this on `opened` / `synchronize` /
   `reopened` and posts the verdict as a PR comment. This is the trigger that
   cannot be missed — a branch's first push is reviewed because the *PR* is the
   event, not the push. It also leaves a durable record on the PR.

Both call the same engine and the same config.

INCREMENTAL REVIEWS
-------------------
Posting a comment leaves machine-readable state in it. On the next run, the
engine reads the last review, diffs only the commits added since, and feeds the
previous findings back to the reviewer — so unchanged code is not re-analysed.
If the previous commit is no longer an ancestor (force-push), it falls back to a
full review against the merge-base, because a rewrite can reintroduce anything.

THIS FILE IS PORTABLE — DO NOT FORK IT
--------------------------------------
The engine is identical in every repo that uses it. Everything repo-specific
lives in `.githooks/review.json`: reviewed paths, authority docs, and the review
axes. Two copies of a *config* cannot drift the way two copies of logic do.

CONFIG (`.githooks/review.json`)
--------------------------------
    {
      "repo": "name shown in logs",
      "description": "one line, used in the reviewer prompt",
      "include_paths": ["src", "docs", "README.md"],
      "exclude_substrings": ["target/", "dist/"],
      "authority": [
        {"path": "docs/architecture.md", "max_chars": 9000},
        {"path": "AGENTS.md", "start": "## Keep docs in sync", "end": "## Secrets",
         "label": "AGENTS.md (docs sync)"}
      ],
      "axes": [{"name": "TERMINOLOGY", "detail": "..."}],
      "fail_guidance": "what earns a fail",
      "model": "deepseek-v4-flash",
      "max_patch": 120000
    }

`start`/`end` slice one section out of a larger file. The end heading line is
included: a boundary token delimits, it does not vanish.

BEHAVIOUR
---------
* No reviewable files in the push  -> exit 0 silently (no API call).
* `AI_REVIEW_SKIP=1`               -> skip entirely.
* API/network failure              -> warn, exit 0 (fail-open). Use
  `AI_REVIEW_STRICT=1` to fail-closed instead.
* `git push --no-verify`           -> git-level bypass of the pre-push trigger
  (it does NOT bypass the CI trigger).
* Every run is appended to `.git/ai_review.log` for auditing.

USAGE
-----
    ai_review.py <remote-name> <remote-url>          # pre-push (stdin refs)
    ai_review.py --range HEAD~1..HEAD                # review an explicit range
    ai_review.py --range main..HEAD --post-comment --repo o/r --pr 12
    ai_review.py --range main..HEAD --no-block       # report only
    ai_review.py --print-conventions                 # what the reviewer sees
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request

API_KEYS = ("DEEPSEEK_CODING_API_KEY", "DEEPSEEK_API_KEY")
DEFAULT_MODEL = "deepseek-v4-flash"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"  # git's empty tree
VERDICT_ORDER = {"pass": 0, "warn": 1, "fail": 2}
CONFIG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "review.json")

#: Hidden markers in a posted comment: one to find our comments at all, one to
#: carry the machine-readable state used by the next incremental review.
COMMENT_MARKER = "<!-- ai-review -->"
STATE_RE = re.compile(r"<!-- ai-review-state: (\{.*?\}) -->", re.S)


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


def run_git(args, cwd=None):
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)


def repo_root() -> str:
    r = run_git(["rev-parse", "--show-toplevel"])
    return r.stdout.strip() or os.getcwd()


# ── config ──────────────────────────────────────────────────────────────────

def load_config() -> dict:
    """Load review.json; fail loudly — a missing config means no review at all."""
    try:
        with open(CONFIG_PATH, encoding="utf-8") as f:
            cfg = json.load(f)
    except OSError as e:
        raise SystemExit(f"[ai-review] cannot read {CONFIG_PATH}: {e}")
    except ValueError as e:
        raise SystemExit(f"[ai-review] invalid JSON in {CONFIG_PATH}: {e}")
    for key in ("repo", "include_paths", "authority", "axes"):
        if key not in cfg:
            raise SystemExit(f"[ai-review] {CONFIG_PATH} is missing required key '{key}'")
    cfg.setdefault("description", "a software repository")
    cfg.setdefault("exclude_substrings", [])
    cfg.setdefault("fail_guidance", "Use 'fail' only for genuine rule violations "
                                    "or real bugs; 'warn' for minor drift.")
    cfg.setdefault("model", DEFAULT_MODEL)
    cfg.setdefault("max_patch", 120000)
    cfg.setdefault("max_authority_chars", 6000)
    return cfg


def is_reviewable(path: str, cfg: dict) -> bool:
    """True when *path* is source/config/docs we want the reviewer to look at."""
    if not path:
        return False
    if any(bad and bad in path for bad in cfg["exclude_substrings"]):
        return False
    if "/.bak" in path or path.endswith(".bak") or ".bak" in os.path.basename(path):
        return False
    for p in cfg["include_paths"]:
        if path == p or path.startswith(p.rstrip("/") + "/"):
            return True
    return False


# ── git plumbing ────────────────────────────────────────────────────────────

def changed_files(base: str, tip: str, cwd: str):
    """Files changed in base..tip (base may be EMPTY_TREE for a new branch)."""
    r = run_git(["diff", "--name-only", "--diff-filter=ACMRT", base, tip], cwd=cwd)
    return [f for f in r.stdout.splitlines() if f.strip()]


def collect_patch(base: str, tip: str, files, cwd: str, max_chars: int):
    """Unified diff for *files* only, truncated to max_chars."""
    if not files:
        return "", False
    r = run_git(["diff", "--no-color", "--unified=3", base, tip, "--", *files], cwd=cwd)
    patch = r.stdout
    if len(patch) > max_chars:
        return patch[:max_chars], True
    return patch, False


def resolve_base(local_sha: str, remote_sha: str, cwd: str) -> str:
    """Best base for the diff; a new remote branch reports an all-zero sha."""
    if remote_sha and set(remote_sha) != {"0"}:
        return remote_sha
    for ref in ("origin/main", "origin/master", "main", "master"):
        r = run_git(["merge-base", local_sha, ref], cwd=cwd)
        if r.returncode == 0 and r.stdout.strip():
            return r.stdout.strip()
    return EMPTY_TREE


def read_push_refs(stdin_lines):
    """Parse pre-push stdin: `<local ref> <local sha> <remote ref> <remote sha>`."""
    refs = []
    for line in stdin_lines:
        parts = line.split()
        if len(parts) != 4:
            continue
        local_ref, local_sha, remote_ref, remote_sha = parts
        refs.append((local_ref, local_sha, remote_ref, remote_sha))
    return refs


def is_deletion(sha: str) -> bool:
    return bool(sha) and set(sha) == {"0"}


def is_ancestor(ancestor: str, descendant: str, cwd: str) -> bool:
    """True when *ancestor* is reachable from *descendant* (so a diff is valid)."""
    if not ancestor or not descendant:
        return False
    r = run_git(["merge-base", "--is-ancestor", ancestor, descendant], cwd=cwd)
    return r.returncode == 0


def is_commit_sha(sha: str, cwd: str) -> bool:
    """True when *sha* is a full 40-hex object that exists in this repo.

    Guards against a corrupted or symbolic state block. A comment written by an
    older build stored the literal string 'HEAD'; diffing from that resolves to
    the current commit, yielding an empty diff and a bogus "nothing to review"
    pass. Anything that is not a real commit object must force a full review.
    """
    if not sha or not re.fullmatch(r"[0-9a-f]{40}", sha):
        return False
    r = run_git(["cat-file", "-e", f"{sha}^{{commit}}"], cwd=cwd)
    return r.returncode == 0


def short(sha: str, n: int = 7) -> str:
    return (sha or "")[:n]


# ── GitHub API (stdlib only) ────────────────────────────────────────────────

def gh_token() -> str:
    for name in ("AI_REVIEW_GH_TOKEN", "GITHUB_TOKEN", "GH_TOKEN"):
        v = os.environ.get(name)
        if v:
            return v
    return ""


def gh_api(method: str, path: str, token: str, data=None):
    """Minimal GitHub REST call. Raises on non-2xx."""
    url = f"https://api.github.com{path}"
    body = json.dumps(data).encode() if data is not None else None
    req = urllib.request.Request(
        url, data=body, method=method,
        headers={
            "Authorization": f"token {token}",
            "Accept": "application/vnd.github+json",
            "User-Agent": "ai-review-hook",
            "Content-Type": "application/json",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as r:
        raw = r.read().decode()
    return json.loads(raw) if raw.strip() else {}


def find_last_review(token: str, repo: str, pr: int):
    """Most recent review-comment state for this PR, or None.

    Newest first: a later comment supersedes an earlier one, so the newest
    committed sha is the only useful base.
    """
    try:
        comments = gh_api("GET", f"/repos/{repo}/issues/{pr}/comments?per_page=100", token)
    except Exception as e:  # noqa: BLE001 - never fail the run over comments
        log(f"[ai-review] could not read PR comments: {e}")
        return None
    if not isinstance(comments, list):
        return None
    for c in reversed(comments):
        body = c.get("body") or ""
        if COMMENT_MARKER not in body:
            continue
        m = STATE_RE.search(body)
        if not m:
            continue
        try:
            state = json.loads(m.group(1))
        except ValueError:
            continue
        state["comment_id"] = c.get("id")
        state["comment_url"] = c.get("html_url")
        return state
    return None


def format_comment(cfg: dict, verdict: dict, base: str, tip: str, files,
                   incremental_from, truncated: bool, patch_chars: int) -> str:
    """Render the PR comment: human-readable verdict + machine-readable state."""
    v = verdict["verdict"]
    emoji = {"pass": "✅", "warn": "⚠️", "fail": "❌"}[v]
    lines = [
        COMMENT_MARKER,
        f"## {emoji} AI review — {v.upper()}",
        "",
        f"**{verdict.get('summary', '') or '(no summary)'}**",
        "",
        "| | |",
        "|---|---|",
        f"| Reviewed commit | `{short(tip)}` |",
        f"| Diff | `{short(base)}..{short(tip)}`"
        + (f" (incremental — since the last review at `{short(incremental_from)}`)"
           if incremental_from else " (full)"),
        f"| Files | {len(files)} |",
        f"| Diff size | {patch_chars} chars{' (truncated)' if truncated else ''} |",
        "",
    ]
    issues = verdict.get("issues") or []
    if issues:
        lines.append(f"### Findings ({len(issues)})")
        lines.append("")
        for it in issues:
            if not isinstance(it, dict):
                continue
            sev = str(it.get("severity", "?")).upper()
            lines.append(f"**`{sev}`** — `{it.get('file', '?')}`")
            lines.append("")
            lines.append(f"{it.get('problem', '')}")
            if it.get("suggestion"):
                lines.append("")
                lines.append(f"> **Fix:** {it['suggestion']}")
            lines.append("")
    else:
        lines.append("No findings.")
        lines.append("")
    lines.append("---")
    lines.append(
        "_Automated review of the diff against this repo's own docs "
        f"(`.githooks/review.json`, model `{cfg['model']}`). "
        "It cannot verify that an issue exists or read the PR body — "
        "the docs-review line is still yours to write._"
    )
    state = {
        "sha": tip,
        "base": base,
        "verdict": v,
        "ts": int(time.time()),
        "files": len(files),
        "issues": len(issues),
        "incremental_from": incremental_from,
    }
    lines.append(f"<!-- ai-review-state: {json.dumps(state, separators=(',', ':'))} -->")
    return "\n".join(lines)


def post_comment(token: str, repo: str, pr: int, body: str) -> str:
    """Post the verdict. Returns the comment URL ('' on failure)."""
    try:
        r = gh_api("POST", f"/repos/{repo}/issues/{pr}/comments", token,
                   {"body": body})
        return r.get("html_url", "")
    except urllib.error.HTTPError as e:
        log(f"[ai-review] post failed: HTTP {e.code} {e.reason}")
        try:
            log(f"[ai-review]   {e.read().decode()[:300]}")
        except Exception:  # noqa: BLE001
            pass
    except Exception as e:  # noqa: BLE001
        log(f"[ai-review] post failed: {e}")
    return ""


# ── authority + prompt ──────────────────────────────────────────────────────

def build_conventions(cwd: str, cfg: dict) -> str:
    """Concatenate the configured authority files (whole or section-sliced)."""
    parts = []
    for item in cfg["authority"]:
        rel = item.get("path", "")
        p = os.path.join(cwd, rel)
        try:
            with open(p, encoding="utf-8") as f:
                text = f.read()
        except OSError:
            continue
        start_tok = item.get("start")
        if start_tok:
            start = text.find(start_tok)
            if start == -1:
                continue
            end_tok = item.get("end")
            if end_tok:
                end = text.find(end_tok, start)
                if end != -1:
                    # Include the end heading itself: the token is a *boundary*,
                    # and a reviewer that never sees the heading cannot tell it
                    # exists. Excluding it produced false "no such section"
                    # findings and hid the last section from the authority.
                    nl = text.find("\n", end)
                    end = nl if nl != -1 else len(text)
                text = text[start:end if end != -1 else start + 12000]
            else:
                text = text[start:start + 12000]
        else:
            text = text[:item.get("max_chars", cfg["max_authority_chars"])]
        label = item.get("label") or rel
        parts.append(f"# {label}\n\n{text.strip()}")
    return "\n\n---\n\n".join(parts)


def build_system_prompt(cfg: dict) -> str:
    """Assemble the reviewer prompt from the config's axes."""
    n = len(cfg["axes"])
    head = (
        f"You are a strict but pragmatic senior reviewer for {cfg['description']} "
        f"(repository `{cfg['repo']}`). You are given that repository's own "
        f"conventions (the authority files below) and a change set about to be "
        f"pushed. Judge ONLY these {n} things:\n"
    )
    axes = "".join(
        f"  {i}. {a['name']}. {a['detail']}\n"
        for i, a in enumerate(cfg["axes"], start=1)
    )
    tail = (
        "Do NOT nitpick style, formatting, comment wording or naming preferences "
        "that the authority files do not state. Do NOT invent issues. If the "
        "change is consistent with the docs and safe, say so.\n"
        "Respond with STRICT JSON only, no markdown fences, in this shape:\n"
        '{"verdict":"pass|warn|fail","summary":"one sentence",'
        '"issues":[{"file":"path","severity":"high|medium|low",'
        '"problem":"what is wrong","suggestion":"how to fix"}]}\n'
        f"{cfg['fail_guidance']}"
    )
    return head + axes + tail


# ── API ─────────────────────────────────────────────────────────────────────

def load_api_key():
    """Return (env_name, key) from the environment or ~/.openclaw/.env."""
    for name in API_KEYS:
        if os.environ.get(name):
            return name, os.environ[name]
    envp = os.path.expanduser("~/.openclaw/.env")
    if os.path.exists(envp):
        try:
            with open(envp, encoding="utf-8") as f:
                for line in f:
                    line = line.strip()
                    if not line or line.startswith("#") or "=" not in line:
                        continue
                    k, _, v = line.partition("=")
                    k = k.strip()
                    v = v.strip().strip('"').strip("'")
                    if k in API_KEYS and v:
                        return k, v
        except OSError:
            pass
    return None, None


def call_deepseek(key: str, model: str, system_prompt: str, conventions: str,
                  patch: str, files, timeout: int, prior: dict | None = None):
    """Call DeepSeek and return the raw reply text.

    `deepseek-v4-flash` is a *reasoning* model: with a modest max_tokens it can
    spend the whole budget on chain-of-thought and return an EMPTY string, which
    looks like a silent failure. We disable the reasoning phase explicitly and
    keep a generous token budget for the JSON.
    """
    from openai import OpenAI

    client = OpenAI(api_key=key, base_url="https://api.deepseek.com")
    user = (
        "## Repository conventions (authoritative)\n\n"
        f"{conventions}\n\n"
    )
    if prior:
        prev_issues = prior.get("issues") or []
        user += (
            "## Previous review on this pull request\n\n"
            f"A previous review at commit `{short(prior.get('sha', ''))}` returned "
            f"**{str(prior.get('verdict', '?')).upper()}**. The change set below "
            "contains ONLY the commits added since then.\n\n"
            "Previously reported findings:\n"
            + ("\n".join(
                f"- [{str(i.get('severity', '?')).upper()}] {i.get('file', '?')}: "
                f"{i.get('problem', '')}"
                for i in prev_issues if isinstance(i, dict)
            ) if prev_issues else "- (none)")
            + "\n\nJudge the NEW change set. Do not repeat a finding that this "
            "change set has fixed. DO report a previous finding that is still "
            "unaddressed, and flag any new problem the change set introduces.\n\n"
        )
    user += (
        "## Change set under review\n\n"
        f"Files ({len(files)}):\n" + "\n".join(f"- {f}" for f in files) + "\n\n"
        "```diff\n" + patch + "\n```\n"
    )
    kwargs = dict(
        model=model,
        messages=[
            {"role": "system", "content": system_prompt},
            {"role": "user", "content": user},
        ],
        temperature=0,
        max_tokens=4000,
        timeout=timeout,
    )
    try:
        kwargs["reasoning_effort"] = "none"
        resp = client.chat.completions.create(**kwargs)
    except TypeError:
        # Older/newer SDK builds may not accept reasoning_effort; retry without.
        kwargs.pop("reasoning_effort", None)
        resp = client.chat.completions.create(**kwargs)

    choice = resp.choices[0]
    text = choice.message.content or ""
    if not text.strip():
        finish = getattr(choice, "finish_reason", "?")
        raise RuntimeError(
            f"model returned empty content (finish_reason={finish}); "
            "the reasoning phase may have consumed the token budget"
        )
    return text


# ── verdict + reporting ─────────────────────────────────────────────────────

def parse_verdict(text: str) -> dict:
    """Parse the model reply into a verdict dict; never raises."""
    t = (text or "").strip()
    if t.startswith("```"):
        t = t.split("```")[1] if "```" in t[3:] else t
        t = t.lstrip("json").strip()
    start, end = t.find("{"), t.rfind("}")
    if start != -1 and end != -1 and end > start:
        t = t[start:end + 1]
    try:
        data = json.loads(t)
    except (ValueError, TypeError):
        return {"verdict": "warn", "summary": "reviewer returned unparseable output",
                "issues": [], "raw": (text or "")[:500]}
    if not isinstance(data, dict):
        return {"verdict": "warn", "summary": "reviewer returned non-object JSON",
                "issues": []}
    v = str(data.get("verdict", "warn")).lower().strip()
    if v not in VERDICT_ORDER:
        v = "warn"
    data["verdict"] = v
    data.setdefault("summary", "")
    issues = data.get("issues")
    data["issues"] = issues if isinstance(issues, list) else []
    return data


def report(verdict: dict) -> None:
    log("")
    log(f"[ai-review] verdict: {verdict['verdict'].upper()} — {verdict.get('summary', '')}")
    for it in verdict.get("issues") or []:
        if not isinstance(it, dict):
            continue
        sev = str(it.get("severity", "?")).upper()
        log(f"[ai-review]   [{sev}] {it.get('file', '?')}")
        log(f"[ai-review]         {it.get('problem', '')}")
        if it.get("suggestion"):
            log(f"[ai-review]         -> {it['suggestion']}")
    log("")


def append_log(cwd: str, entry: dict) -> None:
    try:
        r = run_git(["rev-parse", "--git-dir"], cwd=cwd)
        gitdir = r.stdout.strip() or ".git"
        if not os.path.isabs(gitdir):
            gitdir = os.path.join(cwd, gitdir)
        with open(os.path.join(gitdir, "ai_review.log"), "a", encoding="utf-8") as f:
            f.write(json.dumps(entry, ensure_ascii=False) + "\n")
    except OSError:
        pass


def review_range(base: str, tip: str, cwd: str, args, cfg: dict, system_prompt: str,
                 prior: dict | None = None) -> int:
    """Core: review base..tip, optionally posting the verdict to a PR."""
    files = [f for f in changed_files(base, tip, cwd) if is_reviewable(f, cfg)]
    if not files:
        log("[ai-review] no reviewable changes — skipping")
        if args.post_comment:
            body = format_comment(
                cfg, {"verdict": "pass", "summary": "No reviewable changes in this diff.",
                      "issues": []}, base, tip, [], None, False, 0)
            url = post_comment(gh_token(), args.repo, args.pr, body)
            if url:
                log(f"[ai-review] comment posted: {url}")
        return 0

    patch, truncated = collect_patch(base, tip, files, cwd, args.max_patch)
    if not patch.strip():
        log("[ai-review] empty patch — skipping")
        return 0

    key_env, key = load_api_key()
    if not key:
        log(f"[ai-review] no API key ({'/'.join(API_KEYS)}) — skipping")
        return 0 if not args.strict else 1

    log(f"[ai-review] reviewing {len(files)} file(s) with {args.model} "
        f"({'truncated patch: ' if truncated else ''}{len(patch)} chars)"
        + (f" [incremental since {short(prior.get('sha', ''))}]" if prior else " [full]"))
    try:
        raw = call_deepseek(key, args.model, system_prompt,
                            build_conventions(cwd, cfg), patch, files,
                            args.timeout, prior=prior)
    except Exception as e:  # noqa: BLE001 - never break the push on API issues
        log(f"[ai-review] API call failed: {e}")
        append_log(cwd, {"ts": int(time.time()), "base": base, "tip": tip,
                         "files": files, "error": str(e)[:300]})
        if args.strict:
            log("[ai-review] strict mode -> blocking")
            return 1
        log("[ai-review] fail-open -> allowing the push")
        return 0

    verdict = parse_verdict(raw)
    report(verdict)
    append_log(cwd, {"ts": int(time.time()), "base": base, "tip": tip,
                     "files": files, "verdict": verdict["verdict"],
                     "base_kind": "incremental" if prior else "full",
                     "summary": verdict.get("summary", ""),
                     "issues": verdict.get("issues", [])})

    if args.post_comment:
        body = format_comment(cfg, verdict, base, tip, files,
                              prior.get("sha") if prior else None,
                              truncated, len(patch))
        url = post_comment(gh_token(), args.repo, args.pr, body)
        if url:
            log(f"[ai-review] comment posted: {url}")
        elif args.strict:
            log("[ai-review] strict mode: comment could not be posted -> blocking")
            return 1

    if verdict["verdict"] == "fail" and not args.no_block:
        log("[ai-review] FAIL. Fix the issues above, or bypass with:")
        log("[ai-review]   git push --no-verify")
        log("[ai-review]   AI_REVIEW_SKIP=1 git push")
        return 1
    return 0


def resolve_incremental_base(tip: str, cwd: str, cfg: dict, args, token: str):
    """Pick the base + prior state for an incremental review.

    Returns (base, prior). prior is None when a full review is needed: no
    earlier comment, no token, incremental disabled, or the previously reviewed
    commit is no longer an ancestor (force-push — a rewrite can reintroduce
    anything, so re-review the whole diff).
    """
    if not args.incremental:
        return None, None
    if not (args.post_comment and token and args.repo and args.pr):
        return None, None
    prev = find_last_review(token, args.repo, args.pr)
    if not prev:
        log("[ai-review] no earlier review on this PR — full review")
        return None, None
    sha = prev.get("sha") or ""
    if not is_commit_sha(sha, cwd):
        log(f"[ai-review] previous state sha {sha!r} is not a commit in this repo "
            "— full review")
        return None, None
    if sha == tip:
        log(f"[ai-review] commit {short(tip)} already reviewed and unchanged")
        return None, None
    if not is_ancestor(sha, tip, cwd):
        log(f"[ai-review] previous commit {short(sha)} is not an ancestor of "
            f"{short(tip)} (force-push?) — full review")
        return None, None
    log(f"[ai-review] incremental: {short(sha)}..{short(tip)} "
        f"(previous verdict {str(prev.get('verdict', '?')).upper()})")
    return sha, prev


def main() -> int:
    ap = argparse.ArgumentParser(description="Review a change set against this repo's docs.")
    ap.add_argument("remote", nargs="?", help="remote name (pre-push hook)")
    ap.add_argument("url", nargs="?", help="remote url (pre-push hook)")
    ap.add_argument("--range", dest="rev_range", help="review A..B manually")
    ap.add_argument("--no-block", action="store_true", help="report only")
    ap.add_argument("--strict", action="store_true",
                    help="fail-closed when the API is unavailable")
    ap.add_argument("--print-conventions", action="store_true",
                    help="dump the authority text the reviewer receives")
    ap.add_argument("--post-comment", action="store_true",
                    help="post the verdict as a comment on the pull request")
    ap.add_argument("--repo", default=os.environ.get("AI_REVIEW_REPO", ""),
                    help="owner/name for --post-comment")
    ap.add_argument("--pr", type=int, default=int(os.environ.get("AI_REVIEW_PR") or 0),
                    help="pull request number for --post-comment")
    ap.add_argument("--incremental", dest="incremental", action="store_true",
                    default=os.environ.get("AI_REVIEW_INCREMENTAL", "1") != "0",
                    help="diff only since the last posted review (default on)")
    ap.add_argument("--no-incremental", dest="incremental", action="store_false",
                    help="review the whole diff even if a review exists")
    ap.add_argument("--model", default=None)
    ap.add_argument("--timeout", type=int,
                    default=int(os.environ.get("AI_REVIEW_TIMEOUT", "180")))
    ap.add_argument("--max-patch", type=int, default=None)
    args = ap.parse_args()

    if os.environ.get("AI_REVIEW_SKIP") == "1" or os.environ.get("NOVA_SKIP_AI_REVIEW") == "1":
        log("[ai-review] skipped by environment (AI_REVIEW_SKIP=1)")
        return 0

    cfg = load_config()
    args.model = args.model or os.environ.get("AI_REVIEW_MODEL") or cfg["model"]
    if args.max_patch is None:
        args.max_patch = int(os.environ.get("AI_REVIEW_MAX_PATCH") or cfg["max_patch"])
    system_prompt = build_system_prompt(cfg)

    if args.post_comment and not (args.repo and args.pr):
        raise SystemExit("[ai-review] --post-comment needs --repo owner/name and --pr N")

    cwd = repo_root()
    token = gh_token() if args.post_comment else ""

    if args.print_conventions:
        print(build_conventions(cwd, cfg))
        return 0

    if args.rev_range:
        rng = args.rev_range
        if "..." in rng:
            # Triple-dot means "symmetric difference": review the commits on the
            # tip side since it diverged. Resolve it to the real merge-base
            # rather than trusting a caller to do it — parsing it as a
            # double-dot yields a nonsense ref like '.abc'.
            left, _, right = rng.partition("...")
            left = left or "main"
            right = right or "HEAD"
            r = run_git(["merge-base", left, right], cwd=cwd)
            base = r.stdout.strip() or left
            tip = right
            log(f"[ai-review] triple-dot range -> {short(base)}..{tip} (merge-base)")
        elif ".." in rng:
            base, _, tip = rng.partition("..")
            base = base or "HEAD~1"
            tip = tip or "HEAD"
        else:
            base, tip = f"{rng}~1", rng
        tip_sha = run_git(["rev-parse", tip], cwd=cwd).stdout.strip() or tip
        inc_base, prior = resolve_incremental_base(tip_sha, cwd, cfg, args, token)
        if inc_base:
            base = inc_base
        # Pass the RESOLVED sha, not the symbolic ref. The posted state block is
        # what the next run diffs from, and "HEAD" is meaningless on a later
        # run (or in CI, where HEAD is a different commit).
        return review_range(base, tip_sha, cwd, args, cfg, system_prompt, prior=prior)

    refs = read_push_refs(sys.stdin.read().splitlines())
    if not refs:
        return 0
    rc = 0
    for _local_ref, local_sha, _remote_ref, remote_sha in refs:
        if is_deletion(local_sha):
            continue  # branch/tag deletion — there is no tree to review
        # NOTE: an all-zero *remote* sha means a NEW branch, not a deletion.
        # resolve_base() handles it by falling back to the merge-base with the
        # default branch. Skipping on it here silently unreviewed every first
        # push of a feature branch — which is most pushes.
        base = resolve_base(local_sha, remote_sha, cwd)
        rc = review_range(base, local_sha, cwd, args, cfg, system_prompt) or rc
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
