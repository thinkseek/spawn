#!/bin/sh
# scripts/blackbox/test-serve-log-restart.sh: a restarted serve's port is
# read from its own launch, never from the stopped serve's line in the same
# log (046's step (5) "answered 000000", twice; cc-astra A17).
#
#   scripts/blackbox/test-serve-log-restart.sh [runs]   # default 300; touches nothing outside its scratch
#
# Two processes, as the scenarios have them: the shell launches a fake
# `serve` in the background and reads the log for its port line while the
# child starts. The log already holds the stopped serve's line (port 1111);
# the new launch prints port 2222. Each launch form runs <runs> times,
# pinned to one CPU when taskset is there (the child then competes with the
# reading shell, as on a loaded gate host):
#   old: `serve >"$log" &` and an immediate read (the child truncates the
#        log after the fork, so the shell may read 1111 first);
#   new: `: >"$log"` in the shell, then `serve >>"$log" &` (041, 045, 046
#        and 048's start_serve).
# The test fails if the new form ever reads 1111. The old form's stale reads
# are counted and printed: the race it had, measured on this host.
set -eu
runs=${1:-300}
case $runs in '' | *[!0-9]*) echo "test-serve-log-restart: runs must be a number" >&2; exit 2 ;; esac
pin=""
if command -v taskset >/dev/null 2>&1 && taskset -c 0 true 2>/dev/null; then pin="taskset -c 0"; fi
if [ -n "$pin" ] && [ -z "${SERVE_LOG_TEST_PINNED-}" ]; then
    SERVE_LOG_TEST_PINNED=1 exec $pin sh "$0" "$runs"
fi
scratch=$(mktemp -d "${TMPDIR:-/tmp}/serve-log-test.XXXXXX")
trap 'rm -f "$scratch"/serve "$scratch"/serve.log; rmdir "$scratch"' EXIT
log=$scratch/serve.log
cat >"$scratch/serve" <<'EOF'
#!/bin/sh
echo "serve: http://127.0.0.1:2222 web x auth none"
exec sleep 30
EOF
chmod 755 "$scratch/serve"

read_port() { # the first port line in the log, waiting up to 5 s
    rp_i=0
    while [ "$rp_i" -lt 500 ]; do
        rp_i=$((rp_i + 1))
        rp_port=$(sed -n 's#^serve: http://[^ ]*:\([0-9][0-9]*\) .*#\1#p' "$log" 2>/dev/null | head -n 1)
        [ -n "$rp_port" ] && { echo "$rp_port"; return 0; }
        sleep 0.01
    done
    echo none
}
launch_old() { "$scratch/serve" >"$log" 2>&1 & fake=$!; }
launch_new() { : >"$log"; "$scratch/serve" >>"$log" 2>&1 & fake=$!; }


trial() { # <old|new>: how many of the runs read the stopped serve's port
    t_stale=0
    t_n=0
    while [ "$t_n" -lt "$runs" ]; do
        t_n=$((t_n + 1))
        echo "serve: http://127.0.0.1:1111 web x auth none" >"$log"
        "launch_$1"
        t_port=$(read_port)
        kill "$fake" 2>/dev/null || true
        wait "$fake" 2>/dev/null || true
        [ "$t_port" = 2222 ] || t_stale=$((t_stale + 1))
    done
    echo "$t_stale"
}

old=$(trial old)
new=$(trial new)
echo "old form (serve >log &): $old of $runs runs read the stopped serve's port${pin:+ (one CPU)}"
echo "new form (: >log; serve >>log &): $new of $runs runs read the stopped serve's port"
if [ "$new" -ne 0 ]; then
    echo "FAIL the new form read a stopped serve's port"
    exit 1
fi
echo "ok   the new form always read its own launch's port"
