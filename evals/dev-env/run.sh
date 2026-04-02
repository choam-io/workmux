#!/usr/bin/env bash
#
# Eval harness for workmux-dev-env skill.
#
# Runs tasks against a real codespace to verify that an agent
# using the workmux-dev-env skill can complete dev tasks with
# the local-edit/remote-execute pattern.
#
# Usage: ./run.sh [task_name] [--trials N]
#
# Each task is a directory under tasks/ containing:
#   task.yaml   - task definition (prompt, description)
#   setup.sh    - environment setup (runs before pi)
#   grade.sh    - outcome grading (exit 0 = pass, exit 1 = fail)
#
# Results are written to results/<task>/<trial>/

set -euo pipefail

EVAL_DIR="$(cd "$(dirname "$0")" && pwd)"
TASKS_DIR="$EVAL_DIR/tasks"
RESULTS_DIR="$EVAL_DIR/results"
GROUP_WS="$HOME/.local/share/workmux/groups/choam--workmux-target-followup"
WM="$HOME/ghq/github.com/choam-io/workmux__worktrees/workmux-target-followup/target/debug/workmux"

# Read dev_env state from group workspace
DEV_ENV_STATE="$GROUP_WS/.workmux-group.yaml"

get_ssh_host() {
  grep 'ssh_host:' "$DEV_ENV_STATE" | head -1 | awk '{print $2}'
}

get_remote_workdir() {
  grep 'remote_workdir:' "$DEV_ENV_STATE" | head -1 | awk '{print $2}'
}

get_codespace_name() {
  grep 'codespace_name:' "$DEV_ENV_STATE" | head -1 | awk '{print $2}'
}

TRIALS=${TRIALS:-1}
TASK_FILTER="${1:-}"

timestamp() { date -u +"%Y-%m-%dT%H:%M:%SZ"; }

run_task() {
  local task_dir="$1"
  local task_name=$(basename "$task_dir")
  local trial_num="$2"

  echo "=== Task: $task_name (trial $trial_num) ==="

  local result_dir="$RESULTS_DIR/$task_name/trial-$trial_num"
  mkdir -p "$result_dir"

  # Read prompt from task.yaml
  local prompt=$(grep -A100 '^prompt: |' "$task_dir/task.yaml" | tail -n +2 | sed '/^[^ ]/,$d' | sed 's/^  //')
  if [ -z "$prompt" ]; then
    prompt=$(grep '^prompt:' "$task_dir/task.yaml" | sed 's/^prompt: //')
  fi

  echo "$prompt" > "$result_dir/prompt.txt"

  # Setup
  if [ -f "$task_dir/setup.sh" ]; then
    echo "  Running setup..."
    SSH_HOST=$(get_ssh_host) \
    REMOTE_WORKDIR=$(get_remote_workdir) \
    CODESPACE_NAME=$(get_codespace_name) \
    GROUP_WS="$GROUP_WS" \
    bash "$task_dir/setup.sh" > "$result_dir/setup.log" 2>&1
    local setup_rc=$?
    if [ $setup_rc -ne 0 ]; then
      echo "  SETUP FAILED (exit $setup_rc)"
      echo "setup_failed" > "$result_dir/outcome"
      return 1
    fi
  fi

  # Run the agent (pi in non-interactive print mode)
  echo "  Running agent..."
  local start_ts=$(date +%s)

  cd "$GROUP_WS"
  timeout 300 pi --print "$prompt" > "$result_dir/transcript.txt" 2>&1 || true

  local end_ts=$(date +%s)
  local duration=$((end_ts - start_ts))
  echo "  Agent finished in ${duration}s"
  echo "$duration" > "$result_dir/duration"

  # Grade
  if [ -f "$task_dir/grade.sh" ]; then
    echo "  Grading..."
    SSH_HOST=$(get_ssh_host) \
    REMOTE_WORKDIR=$(get_remote_workdir) \
    CODESPACE_NAME=$(get_codespace_name) \
    GROUP_WS="$GROUP_WS" \
    TRANSCRIPT="$result_dir/transcript.txt" \
    bash "$task_dir/grade.sh" > "$result_dir/grade.log" 2>&1
    local grade_rc=$?

    if [ $grade_rc -eq 0 ]; then
      echo "  ✓ PASS"
      echo "pass" > "$result_dir/outcome"
    else
      echo "  ✗ FAIL"
      echo "fail" > "$result_dir/outcome"
    fi
    return $grade_rc
  fi
}

# Main
echo "workmux-dev-env eval harness"
echo "$(timestamp)"
echo "Group workspace: $GROUP_WS"
echo "Trials per task: $TRIALS"
echo ""

# Verify dev env is attached
if ! grep -q "codespace_name:" "$DEV_ENV_STATE" 2>/dev/null; then
  echo "ERROR: No dev environment attached. Run 'workmux group dev-env attach' first."
  exit 1
fi

echo "SSH host: $(get_ssh_host)"
echo "Remote:   $(get_remote_workdir)"
echo ""

pass=0
fail=0
total=0

for task_dir in "$TASKS_DIR"/*/; do
  [ -f "$task_dir/task.yaml" ] || continue
  task_name=$(basename "$task_dir")

  if [ -n "$TASK_FILTER" ] && [ "$task_name" != "$TASK_FILTER" ]; then
    continue
  fi

  for trial in $(seq 1 "$TRIALS"); do
    total=$((total + 1))
    if run_task "$task_dir" "$trial"; then
      pass=$((pass + 1))
    else
      fail=$((fail + 1))
    fi
    echo ""
  done
done

echo "==============================="
echo "Results: $pass/$total passed ($fail failed)"
echo "pass@1: $(echo "scale=0; $pass * 100 / $total" | bc)%"
echo "==============================="
