#!/usr/bin/env bash
#
# AI code review of the staged diff using the Cursor CLI (`agent`).
# Designed to be called from .git-hooks/pre-commit.
#
# Behavior:
#   - Skips silently if `agent` is not installed (so the hook stays usable
#     for teammates without the CLI).
#   - Runs in `--mode=ask` (read-only — won't modify files).
#   - Picks up rules from AGENTS.md automatically.
#   - Default: WARN-only. Set CURSOR_REVIEW_BLOCK=1 to fail the commit on FAIL.
#   - Set CURSOR_REVIEW_SKIP=1 to bypass entirely (e.g. WIP commits).
#
# Override the model with: CURSOR_REVIEW_MODEL="<model-slug>"

set -u

# Git hooks run in a minimal shell — ensure common install locations are in PATH.
for p in "$HOME/.local/bin" "/usr/local/bin"; do
  [[ ":$PATH:" != *":$p:"* ]] && [[ -d "$p" ]] && export PATH="$p:$PATH"
done

if [[ "${CURSOR_REVIEW_SKIP:-0}" == "1" ]]; then
  echo "cursor-review: skipped (CURSOR_REVIEW_SKIP=1)"
  exit 0
fi

if ! command -v agent >/dev/null 2>&1; then
  echo "cursor-review: agent CLI not found — skipping AI review."
  echo ""
  echo "  To install the Cursor agent CLI:"
  echo "    1. Download the installer:  curl -fsSo /tmp/cursor-install.sh https://cursor.com/install"
  echo "    2. Review the script:       less /tmp/cursor-install.sh"
  echo "    3. Run it:                  bash /tmp/cursor-install.sh"
  echo "    4. Authenticate:            agent login"
  echo ""
  exit 0
fi

DIFF=$(git diff --cached --no-color)
if [[ -z "$DIFF" ]]; then
  exit 0
fi

# Hard cap on diff size sent to the model. Massive diffs blow up cost / context.
MAX_BYTES=${CURSOR_REVIEW_MAX_BYTES:-200000}
if (( ${#DIFF} > MAX_BYTES )); then
  echo "cursor-review: staged diff > ${MAX_BYTES} bytes — skipping AI review."
  echo "  Run \`/agent-review\` from the Source Control tab instead."
  exit 0
fi

PROMPT=$(cat <<'EOF'
You are reviewing the staged git diff below.

Apply the project rules in AGENTS.md (especially the Review checklist
section) and any rules in .cursor/rules/.

For each issue found, output exactly one line in this format:
[BLOCKER] `file:line` — description
[WARNING] `file:line` — description
[NIT] `file:line` — description

Output ONLY issue lines. No headers, no summaries, no commentary, no
verdict. If there are no issues, output nothing.

--- staged diff ---
EOF
)

# Determine which model will be used and print it.
if [[ -n "${CURSOR_REVIEW_MODEL:-}" ]]; then
  REVIEW_MODEL="$CURSOR_REVIEW_MODEL"
else
  REVIEW_MODEL="$(agent about 2>/dev/null | grep '^Model' | sed 's/^Model[[:space:]]*//' || echo 'unknown')"
fi
echo "cursor-review: model = $REVIEW_MODEL"

OUT=$(printf '%s\n%s\n' "$PROMPT" "$DIFF" \
  | agent -p \
      --trust \
      --mode=ask \
      --output-format text \
      ${CURSOR_REVIEW_MODEL:+--model "$CURSOR_REVIEW_MODEL"} \
      2>&1)
STATUS=$?

# ── Format, group, and determine verdict deterministically ───────────
RESULT=$(python3 -c "
import sys, textwrap, re

lines = sys.stdin.read().strip().splitlines()

buckets = {'BLOCKER': [], 'WARNING': [], 'NIT': []}
tag_re = re.compile(r'^\[(BLOCKER|WARNING|NIT)\]\s*(.*)')

for raw in lines:
    raw = raw.strip()
    m = tag_re.match(raw)
    if m:
        buckets[m.group(1)].append(m.group(2))

def fmt_issue(text):
    if ' — ' in text:
        ref, _, body = text.partition(' — ')
        out = [ref + ' —']
        out.extend(textwrap.wrap(body, width=76,
                   initial_indent='    ', subsequent_indent='    '))
        return '\n'.join(out)
    return textwrap.fill(text, width=80)

sections = []
for label, key in [('Blockers', 'BLOCKER'), ('Warnings', 'WARNING'), ('Nits', 'NIT')]:
    items = buckets[key]
    if not items:
        continue
    sections.append('## ' + label)
    for item in items:
        sections.append('')
        sections.append('- ' + fmt_issue(item))
    sections.append('')

verdict = 'FAIL' if buckets['BLOCKER'] else 'PASS'
counts = []
for key in ('BLOCKER', 'WARNING', 'NIT'):
    n = len(buckets[key])
    if n:
        counts.append(f'{n} {key.lower()}' + ('s' if n != 1 else ''))
summary = ', '.join(counts) if counts else 'no issues found'

sections.append('## Summary')
sections.append(f'{verdict}: {summary}')
sections.append('')
sections.append(verdict)

print('\n'.join(sections))
" <<< "$OUT")

echo
echo "──────── Cursor AI review ────────"
echo "$RESULT"
echo "──────────────────────────────────"

if [[ $STATUS -ne 0 ]]; then
  echo "cursor-review: agent CLI exited $STATUS — not blocking commit."
  exit 0
fi

VERDICT=$(echo "$RESULT" | grep -oE '^(PASS|FAIL)$' | tail -1)
if [[ "$VERDICT" == "FAIL" ]]; then
  if [[ "${CURSOR_REVIEW_BLOCK:-0}" == "1" ]]; then
    echo "cursor-review: FAIL — blocking commit (CURSOR_REVIEW_BLOCK=1)."
    echo "  Bypass with: CURSOR_REVIEW_SKIP=1 git commit ..."
    exit 1
  fi
  echo "cursor-review: FAIL — warning only. Set CURSOR_REVIEW_BLOCK=1 to enforce."
fi

exit 0
