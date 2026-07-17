#!/usr/bin/env bash
# commit-per-file.sh — one git commit per changed file, then optional push.
#
# Rules:
#   - Only paths that git considers unignored (status --porcelain already
#     omits untracked ignored files; we still re-check with check-ignore).
#   - Never uses `git add -A` / `git add .` on the whole tree.
#   - One path → one commit (subject + body explain that file only).
#   - Does not amend, force-push, or skip hooks.
#
# Usage:
#   ./scripts/commit-per-file.sh           # commit only
#   ./scripts/commit-per-file.sh --push    # commit then push to origin
#   ./scripts/commit-per-file.sh --dry-run # print plan, no commits

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "error: not a git repository: $ROOT" >&2
  exit 1
fi

PUSH=0
DRY=0
for arg in "$@"; do
  case "$arg" in
    --push) PUSH=1 ;;
    --dry-run|--dry) DRY=1 ;;
    -h|--help)
      sed -n '2,20p' "$0"
      exit 0
      ;;
    *)
      echo "unknown option: $arg (use --push, --dry-run, or --help)" >&2
      exit 2
      ;;
  esac
done

# Human-readable commit message for a single path (subject + body).
message_for() {
  local path="$1"
  local subject body

  case "$path" in
    .gitignore)
      subject="chore(gitignore): ignore local docs, plans, and write-test artifacts"
      body="Keep planning notes, MCP skill dumps, and scratch write-test files out of the tree so Stage harness work can ship without local junk."
      ;;
    Cargo.toml)
      subject="build: wire workspace deps for harness Stage transports"
      body="Update the root workspace Cargo.toml for the Stage session stack (external terminal, session gate, related crates)."
      ;;
    crates/rinne-workers/Cargo.toml)
      subject="build(workers): add deps for external terminal and session gate"
      body="Declare worker-crate dependencies needed for Terminal.app launching, process control, and concurrent visible-session caps."
      ;;
    crates/rinne-workers/src/lib.rs)
      subject="feat(workers): export session_gate module"
      body="Surface the visible-session concurrency gate from the workers crate root so adapters can cap Terminal windows."
      ;;
    crates/rinne-workers/src/session_gate.rs)
      subject="feat(workers): cap concurrent visible harness Terminal sessions"
      body="Add a process-wide gate so Stage only opens a limited number of system Terminal windows; overflow falls back to headless."
      ;;
    crates/rinne-workers/src/transport/mod.rs)
      subject="feat(workers): register external_terminal transport module"
      body="Expose the system-Terminal transport alongside PTY and subprocess transports."
      ;;
    crates/rinne-workers/src/transport/external_terminal.rs)
      subject="feat(workers): open real harness product UI in system Terminal"
      body="$(cat <<'EOF'
Launch Claude/Grok/Codex/etc. in Terminal.app (or iTerm) for Stage visibility.

- Interactive product UI without script(1)/tee corrupting mouse tracking
- Result-file handoff so Rinne still parses plans/deliverables
- Soft-stop (SIGTERM + TTY reset) then multi-strategy window close
- Stream deliverable lines back into the Stage event sink
EOF
)"
      ;;
    crates/rinne-workers/src/transport/pty.rs)
      subject="feat(workers): harden PTY path for interactive harness fallback"
      body="Improve embedded PTY handling when the system Terminal cannot be opened, including cleaner cancel/kill behavior."
      ;;
    crates/rinne-workers/src/transport/subprocess.rs)
      subject="feat(workers): add result_file field on SubprocessSpec"
      body="Allow Stage interactive runs to recover deliverables from a result path instead of tee'd stdout (which breaks product TUIs)."
      ;;
    crates/rinne-workers/src/adapters/common.rs)
      subject="feat(workers): Stage interactive TUI + result-file orchestration"
      body="$(cat <<'EOF'
Drive visible harness sessions with the product UI by default:

- Short kickoff + on-disk task/result files under .rinne/stage-prompts
- Prefer interactive argv when Stage is visible; plain single-turn when opted out
- No Terminal retry-on-timeout spam; stream role labels into Stage
EOF
)"
      ;;
    crates/rinne-workers/src/adapters/grok.rs)
      subject="feat(workers): Grok Build interactive fullscreen Stage argv"
      body="Use grok --fullscreen --always-approve --no-plan for product-UI Stage sessions instead of headless streaming-json dumps."
      ;;
    crates/rinne-workers/src/adapters/claude_code.rs)
      subject="feat(workers): Claude Code interactive Stage args and shared helpers"
      body="Align Claude Code with the shared interactive Stage path and common adapter helpers used by other harnesses."
      ;;
    crates/rinne-workers/src/adapters/codex.rs)
      subject="feat(workers): Codex interactive Stage support and streaming parse"
      body="Extend the Codex adapter for visible Stage sessions and richer event mapping."
      ;;
    crates/rinne-workers/src/adapters/opencode.rs)
      subject="feat(workers): OpenCode interactive Stage support"
      body="Wire OpenCode into the interactive Stage path with consistent argv/result handling."
      ;;
    crates/rinne-workers/src/adapters/cursor.rs)
      subject="feat(workers): Cursor agent interactive Stage support"
      body="Add interactive Stage argv paths for cursor-agent alongside headless execution."
      ;;
    crates/rinne-workers/src/adapters/aider.rs)
      subject="feat(workers): Aider interactive Stage support"
      body="Teach the Aider adapter interactive Stage invocation options used by the harness Stage."
      ;;
    crates/rinne-workers/src/adapters/antigravity.rs)
      subject="feat(workers): Antigravity interactive Stage support"
      body="Add interactive Stage hooks for the Antigravity (agy) harness adapter."
      ;;
    crates/rinne-workers/src/adapters/mod.rs)
      subject="feat(workers): export mcp_util from adapters module"
      body="Make MCP provisioning utilities available to harness adapters that provision tools into CLIs."
      ;;
    crates/rinne-workers/src/adapters/mcp_util.rs)
      subject="feat(workers): shared MCP JSON provision helper for harnesses"
      body="Write scoped MCP config for harness CLIs while keeping secrets out of on-disk JSON (env expansion)."
      ;;
    crates/rinne-workers/tests/serves_tools.rs)
      subject="test(workers): update serves_tools for Stage/adapter changes"
      body="Keep MCP/tool-serving adapter tests aligned with the updated harness adapter surface."
      ;;
    crates/rinne-conductor/src/backend.rs)
      subject="feat(conductor): harness planner Stage visibility and auth fail-fast"
      body="$(cat <<'EOF'
When Stage is on, harness conductor runs open visible sessions and forward events.

- Longer planner timeout for real product-UI turns
- Treat not-logged-in transcripts as failure so the chain can fall through
- Prefer a model from the harness ladder when planning
EOF
)"
      ;;
    crates/rinne-conductor/src/conductor.rs)
      subject="feat(conductor): generic harness fallthrough on auth/plan failure"
      body="$(cat <<'EOF'
Try conductor backends in order; on auth or hard failure narrate and move on.

- Skip API planner when backend = harness
- Detect login/auth failure strings without hardcoding a single vendor
- Clearer short error lines in Stage narration
EOF
)"
      ;;
    crates/rinne-cli/src/runner.rs)
      subject="feat(cli): Stage env, conductor harness order, planner event sink"
      body="$(cat <<'EOF'
Wire CLI runs into Harness Stage:

- apply_harness_stage_env for visible sessions in the TUI
- order_harnesses_for_conductor from preferences (not Grok-hardcoded)
- build_conductor_with_events so planner panes stream into Stage
EOF
)"
      ;;
    crates/rinne-cli/src/tui/stage.rs)
      subject="feat(cli): Harness Stage multi-pane session board"
      body="Add Stage session model (open/append/finish/scroll) so live harness transcripts can be shown in the TUI middle region."
      ;;
    crates/rinne-cli/src/tui/mod.rs)
      subject="feat(cli): integrate Stage board into interactive TUI event loop"
      body="Handle SessionOpened/Message/Token events into Stage panes; toggle with /stage and ctrl+y; cycle/scroll focused panes."
      ;;
    crates/rinne-cli/src/tui/ui.rs)
      subject="feat(cli): draw Stage panes and highlight harness output"
      body="Render up to three Stage panes with status chrome; style deliverable headers and success lines for readability."
      ;;
    crates/rinne-cli/src/tui/complete.rs)
      subject="feat(cli): add /stage to slash-command completion"
      body="Surface the stage show/hide command in the interactive completion list."
      ;;
    crates/rinne-cli/src/learn/translate.rs)
      subject="feat(cli): map SessionOpened events in learn translate path"
      body="Keep learn/session translation aware of Stage SessionOpened worker events."
      ;;
    scripts/commit-per-file.sh)
      subject="chore(scripts): add per-file commit helper for Stage ship"
      body="Script creates one conventional commit per changed file, skips gitignored paths, and optionally pushes to origin."
      ;;
    *)
      subject="chore: update $(basename "$path")"
      body="Commit changes to \`${path}\` as part of the Harness Stage work."
      ;;
  esac

  printf '%s\n\n%s\n\nFile: %s\n' "$subject" "$body" "$path"
}

# Collect changed paths. Porcelain omits untracked ignored files by default.
# Bash 3.2 compatible (no mapfile).
PATHS_FILE="$(mktemp "${TMPDIR:-/tmp}/rinne-commit-paths.XXXXXX")"
trap 'rm -f "$PATHS_FILE"' EXIT

count=0
while IFS= read -r line || [[ -n "$line" ]]; do
  [[ -z "$line" ]] && continue
  # Format: XY PATH  or  XY OLD -> NEW
  xy="${line:0:2}"
  rest="${line:3}"

  # Skip pure ignore entries if any appear (status code '!')
  case "$xy" in
    *'!'*) continue ;;
  esac

  path="$rest"
  case "$rest" in
    *" -> "*) path="${rest#* -> }" ;;
  esac
  # Strip optional quotes used by git for special paths
  path="${path#\"}"
  path="${path%\"}"

  [[ -z "$path" ]] && continue

  # Respect ignore rules: never stage ignored paths.
  if git check-ignore -q -- "$path" 2>/dev/null; then
    echo "skip (ignored): $path"
    continue
  fi

  # If untracked, double-check it would not be ignored when added.
  if [[ "$xy" == "??" ]]; then
    if git check-ignore -q --no-index -- "$path" 2>/dev/null; then
      echo "skip (would be ignored): $path"
      continue
    fi
  fi

  printf '%s\n' "$path" >>"$PATHS_FILE"
  count=$((count + 1))
done < <(git status --porcelain=v1 -uall)

if [[ "$count" -eq 0 ]]; then
  echo "nothing to commit after ignore filters (or working tree clean)"
  exit 0
fi

echo "Will create $count commit(s) (1 file each):"
while IFS= read -r p; do
  echo "  - $p"
done <"$PATHS_FILE"
echo

if [[ "$DRY" -eq 1 ]]; then
  echo "[dry-run] no commits created"
  while IFS= read -r p; do
    echo "----- $p -----"
    message_for "$p" | sed 's/^/  /'
    echo
  done <"$PATHS_FILE"
  exit 0
fi

# Ensure nothing is pre-staged so each commit is exactly one file.
if ! git diff --cached --quiet 2>/dev/null; then
  echo "error: index already has staged changes; unstage them first:" >&2
  echo "  git restore --staged ." >&2
  exit 1
fi

n=0
while IFS= read -r path; do
  n=$((n + 1))
  if [[ ! -e "$path" ]] && [[ ! -L "$path" ]]; then
    # Deleted file: still commit the deletion if git knows it.
    if git ls-files --error-unmatch -- "$path" >/dev/null 2>&1; then
      git add -u -- "$path"
    else
      echo "skip (missing, not tracked): $path"
      continue
    fi
  else
    git add -- "$path"
  fi

  # Safety: only this path may be staged.
  staged="$(git diff --cached --name-only)"
  if [[ "$staged" != "$path" ]]; then
    echo "error: expected only '$path' staged, got:" >&2
    echo "$staged" >&2
    git restore --staged . >/dev/null 2>&1 || true
    exit 1
  fi

  msg="$(message_for "$path")"
  echo "[$n/$count] commit $path"
  # Use -m twice for subject + body, or a single HEREDOC via printf.
  git commit -m "$msg"
done <"$PATHS_FILE"

echo
echo "Created $n commit(s) on $(git rev-parse --abbrev-ref HEAD)."
git log --oneline -n "$n"
echo

if [[ "$PUSH" -eq 1 ]]; then
  branch="$(git rev-parse --abbrev-ref HEAD)"
  echo "Pushing $branch → origin …"
  git push -u origin "$branch"
  echo "Push complete."
else
  echo "Local commits only. To push:"
  echo "  git push -u origin $(git rev-parse --abbrev-ref HEAD)"
  echo "Or re-run:"
  echo "  $0 --push"
fi
