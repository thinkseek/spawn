#!/bin/sh
# Universal, source-agnostic gate runner for Spawn missions.
# Contract: universal-gate.sh <repo> <base> <candidate> <scenarios> [<scenario>]
# Writes JSON verdict to $SPAWN_GATE_VERDICT without requiring GitHub or Postgres.

set -eu

if [ "$#" -lt 4 ] || [ "$#" -gt 5 ]; then
    echo "usage: universal-gate.sh <repo> <base> <candidate> <scenarios> [<scenario>]" >&2
    exit 2
fi

REPO="$1"
BASE="$2"
CANDIDATE="$3"
SCENARIOS="$4"
ONLY_SCENARIO="${5:-}"

# Never expose controller or shared test database credentials to candidate code.
unset SPAWN_DATABASE_URL || true
if [ "${SPAWN_GATE_NO_DB:-0}" = "1" ] || [ -f "$SCENARIOS/no-test-database" ]; then
    unset SPAWN_TEST_DATABASE_URL || true
fi

write_verdict() {
    local verdict="$1"
    local reason="$2"
    if [ -n "${SPAWN_GATE_VERDICT:-}" ]; then
        if command -v python3 >/dev/null 2>&1; then
            python3 - "$SPAWN_GATE_VERDICT" "$verdict" "$reason" "$BASE" "$CANDIDATE" <<'PY'
import json, sys
out_path, verdict, reason, base, cand = sys.argv[1:6]
with open(out_path, "w", encoding="utf-8") as fh:
    json.dump(
        {"v": 1, "verdict": verdict, "reason": reason, "base": base, "candidate": cand},
        fh,
    )
PY
        else
            local safe_reason
            safe_reason="$(printf '%s' "$reason" | tr '\n\r' '  ' | sed 's/\\/\\\\/g; s/"/\\"/g')"
            printf '{"v":1,"verdict":"%s","reason":"%s","base":"%s","candidate":"%s"}\n' \
                "$verdict" "$safe_reason" "$BASE" "$CANDIDATE" > "$SPAWN_GATE_VERDICT"
        fi
    fi
}

SCRATCH_ROOT="${TMPDIR:-/tmp}"
WORKTREE="$(mktemp -d "${SCRATCH_ROOT%/}/spawn-gate-XXXXXX")"
cleanup() {
    rm -rf "$WORKTREE"
}
trap cleanup EXIT

CANDIDATE_DIR="$WORKTREE/candidate"
mkdir -p "$CANDIDATE_DIR"
git --git-dir="$REPO" archive "$CANDIDATE" | tar -x -C "$CANDIDATE_DIR"

export SPAWN_GATE_REPO="$REPO"
export SPAWN_GATE_BASE="$BASE"
export SPAWN_GATE_CANDIDATE="$CANDIDATE"
export SPAWN_GATE_TREE="$CANDIDATE_DIR"
export SPAWN_GATE_SCENARIOS="$SCENARIOS"

run_scenario() {
    script="$1"
    script_name="$(basename "$script")"
    echo "universal-gate: running $script_name for candidate $CANDIDATE"
    set +e
    (
        cd "$CANDIDATE_DIR"
        if command -v bash >/dev/null 2>&1; then
            bash "$script" "$CANDIDATE_DIR" "$BASE" "$CANDIDATE" "$REPO"
        else
            sh "$script" "$CANDIDATE_DIR" "$BASE" "$CANDIDATE" "$REPO"
        fi
    )
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
        write_verdict "rejected" "$script_name failed (exit $rc)"
        exit 0
    fi
}

if [ -n "$ONLY_SCENARIO" ]; then
    if [ -f "$ONLY_SCENARIO" ]; then
        run_scenario "$ONLY_SCENARIO"
    elif [ -f "$SCENARIOS/$ONLY_SCENARIO" ]; then
        run_scenario "$SCENARIOS/$ONLY_SCENARIO"
    else
        write_verdict "rejected" "requested scenario '$ONLY_SCENARIO' not found in $SCENARIOS"
        exit 0
    fi
elif [ -f "$SCENARIOS/verify.sh" ]; then
    run_scenario "$SCENARIOS/verify.sh"
else
    CHECKS_LIST="$WORKTREE/checks.list"
    find "$SCENARIOS" -maxdepth 1 -type f -name 'check-*.sh' | LC_ALL=C sort > "$CHECKS_LIST"
    if [ ! -s "$CHECKS_LIST" ]; then
        # Default sanity check when scenarios directory contains no script: require
        # non-empty tree delta from base (or at least valid candidate archive).
        if [ "$BASE" = "$CANDIDATE" ]; then
            write_verdict "rejected" "candidate equals base commit with no scenario scripts"
            exit 0
        fi
        write_verdict "accepted" "candidate archived cleanly with no failing scenarios"
        exit 0
    fi
    while IFS= read -r script; do
        [ -n "$script" ] || continue
        run_scenario "$script"
    done < "$CHECKS_LIST"
fi

write_verdict "accepted" "all scenario checks passed"
exit 0
