#!/bin/sh
# `spawn tick` and the workspace store.
#
# One start command is submitted, then one tick must: export the base tree
# under $SPAWN_WORKSPACE_ROOT/<agent>, keep .git out of it, type the prompt
# into the tmux pane, and report commands=1 and a session count. A second
# tick must not start a second execution.

capture "$SPAWN_BIN" init-db
assert_eq "$status" 0 "init-db exit"
assert_contains "$(out)" "schema installed" "init-db output"

# The local tmux host runs the program named by SPAWN_TMUX_PROGRAM and adds no
# socket of its own, while assertions.sh reads SPAWN_TMUX_SOCKET. Pin the scenario's
# private socket in a wrapper so the tick and the pane helpers share a server.
cat >"$WORK/tmux" <<EOF
#!/bin/sh
exec tmux -L '$SPAWN_TMUX_SOCKET' "\$@"
EOF
chmod 755 "$WORK/tmux"
export SPAWN_TMUX_PROGRAM="$WORK/tmux"

# One counter from the single tick report line
# `commands=N observations=N sessions=N`.
tick_count() {
    out | tr ' ' '\n' | sed -n "s/^$1=//p"
}

# A start action whose engine name is unknown is rejected before the store.
capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k_bad \
    --action '{"type":"start","engine":"nope","prompt":"x"}'
assert_eq "$status" 1 "unknown engine exit"
assert_contains "$(err)" "--action is not a command action" "unknown engine message"

# Claude Code is the only engine: a start naming codex is refused the same way.
capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k_codex \
    --action '{"type":"start","engine":"codex","prompt":"x"}'
assert_eq "$status" 1 "codex engine exit"
assert_contains "$(err)" "unknown engine codex" "codex engine message"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 0 "no command was written"

# tick needs a repository and a root; the environment supplies both, so a
# call with them removed must fail with the documented message.
capture sh -c 'unset SPAWN_GIT_REPO SPAWN_WORKSPACE_ROOT; exec "$0" tick' "$SPAWN_BIN"
assert_eq "$status" 1 "tick without repo or root exit"
assert_contains "$(err)" "--repo or SPAWN_GIT_REPO is required" "tick missing repo message"

capture "$SPAWN_BIN" submit --agent agent_ws --session session_ws --key k1 \
    --action '{"type":"start","engine":"claude","prompt":"probe workspace 030"}'
assert_eq "$status" 0 "submit exit"
assert_contains "$(out)" "command=" "submit output"
assert_eq "$(scalar 'select count(*) from spawn_commands')" 1 "one command was written"

# The base branch is exported to <root>/<agent>; the directory is named from
# BranchName::for_agent, so the agent id is the directory name.
workspace="$SPAWN_WORKSPACE_ROOT/agent_ws"

capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "first tick exit"
assert_eq "$(tick_count commands)" 1 "first tick handles one command"
assert_eq "$(tick_count sessions)" 1 "first tick counts the submitted session"

assert_file "$workspace/README.md"
assert_eq "$(cat "$workspace/README.md")" "base" "workspace holds the base content"
assert_no_file "$workspace/.git"

# The prompt is typed into the pane; the fake engine echoes every line.
pane_shows_prompt() {
    pane | grep -F "echo: probe workspace 030" >/dev/null
}
wait_for 60 pane_shows_prompt || fail "prompt text never reached the pane: $(pane 2>&1)"

assert_eq "$(scalar 'select count(*) from agent_executions')" 1 "one execution row"
assert_eq "$(scalar "select state from spawn_commands where idempotency_key = 'k1'")" \
    "finished" "command is finished"
assert_eq "$(scalar "select succeeded from spawn_commands where idempotency_key = 'k1'")" \
    "t" "command succeeded"

# A second tick finds no open command and leaves the running execution alone.
capture "$SPAWN_BIN" tick
assert_eq "$status" 0 "second tick exit"
assert_eq "$(tick_count commands)" 0 "second tick handles no command"
assert_eq "$(tick_count sessions)" 1 "second tick still counts the session"
assert_eq "$(scalar 'select count(*) from agent_executions')" 1 \
    "second tick starts no second execution"
assert_eq "$(tmux -L "$SPAWN_TMUX_SOCKET" list-sessions -F '#{session_name}' | wc -l | tr -d ' ')" \
    1 "one terminal for one execution"

# The execution is still alive, so the mount stays in place.
assert_file "$workspace/README.md"
assert_eq "$(cat "$workspace/README.md")" "base" "workspace still holds the base content"
