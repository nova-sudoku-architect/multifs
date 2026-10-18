#!/usr/bin/env bash
# install.sh — enable this repo's client-side git hooks.
#
# Points git at the version-controlled .githooks/ directory (rather than the
# default .git/hooks, which is not tracked), so the pre-push review applies to
# every clone that runs this once.
#
# The review is configured by .githooks/review.json — edit that, never the engine.
#
# Usage:  bash .githooks/install.sh
# Undo:   git config --unset core.hooksPath
set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

chmod +x .githooks/pre-push .githooks/ai_review.py
git config core.hooksPath .githooks

echo "installed: core.hooksPath = $(git config --get core.hooksPath)"
echo
echo "pre-push behaviour:"
echo "  * reviews the paths listed in include_paths in .githooks/review.json"
echo "  * skips $(python3 -c "import json;print(', '.join(json.load(open('.githooks/review.json'))['exclude_substrings']))" 2>/dev/null || echo 'configured exclusions')"
echo "  * blocks on a 'fail' verdict"
echo
echo "bypass a single push:   git push --no-verify"
echo "skip the review only:   AI_REVIEW_SKIP=1 git push"
echo "review without pushing: python3 .githooks/ai_review.py --range HEAD~1..HEAD"
echo "see what it judges:     python3 .githooks/ai_review.py --print-conventions"
echo "disable entirely:       git config --unset core.hooksPath"
