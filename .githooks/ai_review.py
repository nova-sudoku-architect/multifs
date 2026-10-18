#!/usr/bin/env python3
"""ai_review.py — pre-push review of a change set against this repo's own docs.

WHY THIS EXISTS
---------------
A repository's rules live in markdown, and code drifts away from them silently.
A stale doc makes the next reader — human or agent — do the wrong thing. So
before a push, an LLM compares the diff against the repo's authority files and
returns a verdict. A `fail` verdict blocks the push.

THIS FILE IS PORTABLE — DO NOT FORK IT
--------------------------------------
The engine is identical in every repo that uses it. Everything repo-specific
lives in `.githooks/review.json`: which paths are reviewed, which docs are the
authority, and what the review axes are. That split is deliberate: two copies of
the same logic drift apart (this repo already paid for that lesson with a
duplicated holiday calendar), whereas two copies of a *config* cannot.

To adopt in another repo: copy `ai_review.py`, `pre-push` and `install.sh`
verbatim, and write a `review.json`. Do not edit the engine to special-case a
repo — add an axis or an authority file to its config instead.

CONFIG (`.githooks/review.json`)
--------------------------------
    {
      "repo": "name shown in logs",
      "description": "one line, used in the reviewer prompt",
      "include_paths": ["src", "docs", "README.md"],     # reviewed paths
      "exclude_substrings": ["target/", "dist/"],        # never reviewed
      "authority": [                                     # what the review judges
        {"path": "docs/architecture.md", "max_chars": 9000},
        {"path": "AGENTS.md", "start": "## Keep docs in sync", "end": "## Secrets",
         "label": "AGENTS.md (docs sync)"}
      ],
      "axes": [{"name": "TERMINOLOGY", "detail": "..."}],
      "fail_guidance": "one sentence: what earns a fail",
      "model": "deepseek-v4-flash",
      "max_patch": 120000
    }

`start`/`end` slice one section out of a larger file, so a single normative
section of an agent doc can be authority without feeding the whole file.

BEHAVIOUR
---------
* No reviewable files in the push  -> exit 0 silently (no API call).
* `AI_REVIEW_SKIP=1`               -> skip entirely.
* API/network failure              -> warn, exit 0 (fail-open). Use
  `AI_REVIEW_STRICT=1` to fail-closed instead.
* `git push --no-verify`           -> git-level bypass.
* Every run is appended to `.git/ai_review.log` for auditing.

This hook is *client-side* and therefore advisory. It cannot prove an issue
exists, and it can be bypassed. Server-side branch protection is the only real
enforcement.

USAGE
-----
    ai_review.py <remote-name> <remote-url>      # pre-push hook mode (stdin refs)
    ai_review.py --range HEAD~1..HEAD            # review an explicit range
    ai_review.py --range main..HEAD --no-block   # report only
    ai_review.py --print-conventions             # show what the reviewer sees
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time

API_KEYS = ("DEEPSEEK_CODING_API_KEY", "DEEPSEEK_API_KEY")
DEFAULT_MODEL = "deepseek-v4-flash"
EMPTY_TREE = "4b825dc642cb6eb9a060e54bf8d69288fbee4904"  # git's empty tree
VERDICT_ORDER = {"pass": 0, "warn": 1, "fail": 2}
CONFIG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "review.json")


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


SYSTEM_PROMPT = None  # populated in main() from review.json


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
                  patch: str, files, timeout: int):
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


def review_range(base: str, tip: str, cwd: str, args, cfg: dict, system_prompt: str) -> int:
    """Core: review base..tip. Returns a process exit code."""
    files = [f for f in changed_files(base, tip, cwd) if is_reviewable(f, cfg)]
    if not files:
        log("[ai-review] no reviewable changes — skipping")
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
        f"({'truncated patch: ' if truncated else ''}{len(patch)} chars)")
    try:
        raw = call_deepseek(key, args.model, system_prompt,
                            build_conventions(cwd, cfg), patch, files, args.timeout)
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
                     "summary": verdict.get("summary", ""),
                     "issues": verdict.get("issues", [])})

    if verdict["verdict"] == "fail" and not args.no_block:
        log("[ai-review] BLOCKED. Fix the issues above, or bypass with:")
        log("[ai-review]   git push --no-verify")
        log("[ai-review]   AI_REVIEW_SKIP=1 git push")
        return 1
    return 0


def main() -> int:
    ap = argparse.ArgumentParser(description="Pre-push review against this repo's docs.")
    ap.add_argument("remote", nargs="?", help="remote name (pre-push hook)")
    ap.add_argument("url", nargs="?", help="remote url (pre-push hook)")
    ap.add_argument("--range", dest="rev_range", help="review A..B manually")
    ap.add_argument("--no-block", action="store_true", help="report only")
    ap.add_argument("--strict", action="store_true",
                    help="fail-closed when the API is unavailable")
    ap.add_argument("--print-conventions", action="store_true",
                    help="dump the authority text the reviewer receives")
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

    cwd = repo_root()

    if args.print_conventions:
        print(build_conventions(cwd, cfg))
        return 0

    if args.rev_range:
        if ".." in args.rev_range:
            base, _, tip = args.rev_range.partition("..")
            base = base or "HEAD~1"
            tip = tip or "HEAD"
        else:
            base, tip = f"{args.rev_range}~1", args.rev_range
        return review_range(base, tip, cwd, args, cfg, system_prompt)

    refs = read_push_refs(sys.stdin.read().splitlines())
    if not refs:
        return 0
    rc = 0
    for _local_ref, local_sha, _remote_ref, remote_sha in refs:
        if is_deletion(local_sha):
            continue  # branch/tag deletion — there is no tree to review
        # NOTE: an all-zero *remote* sha means a NEW branch, not a deletion.
        # resolve_base() handles it by falling back to the merge-base with the
        # default branch. Skipping on it here silently unreviewed every
        # first push of a feature branch — which is most pushes.
        base = resolve_base(local_sha, remote_sha, cwd)
        rc = review_range(base, local_sha, cwd, args, cfg, system_prompt) or rc
    return rc


if __name__ == "__main__":
    raise SystemExit(main())
