#!/bin/sh
# check-runtime-dir.sh COMMAND [ARG...]: run COMMAND where it can neither
# write in nor remove from the real runtime directory (/run/user/<uid>), and
# say whether it would have (backlog S20).
#
# COMMAND runs in a bwrap whose /run/user/<uid> is a fresh tmpfs (0700),
# with XDG_RUNTIME_DIR pointing at it, as a login sets it. Before COMMAND,
# the tmpfs gets one sentinel of each kind a run could remove there: a
# launch's environment file, a leaders record, a gpg socket directory.
# After COMMAND, the tmpfs must hold exactly the sentinels: anything new is
# something COMMAND wrote there, a missing one something it removed.
#
# Exit: COMMAND's status when the tmpfs is as seeded; 3 when it is not (the
# difference on stderr); 2 when the check itself cannot run.
set -eu

[ "$#" -gt 0 ] || { echo "usage: check-runtime-dir.sh COMMAND [ARG...]" >&2; exit 2; }
command -v bwrap >/dev/null 2>&1 || { echo "check-runtime-dir: bwrap is not installed" >&2; exit 2; }
uid=$(id -u)
runtime=/run/user/$uid
[ -d "$runtime" ] || { echo "check-runtime-dir: no $runtime to stand in for" >&2; exit 2; }

# The inner script: seed, run, compare. Its arguments are COMMAND's.
inner='
set -u
runtime=$1
shift
mkdir -m 700 "$runtime/spawn-leaders-sentinel" "$runtime/gnupg" "$runtime/gnupg/d.sentinel"
: >"$runtime/spawn-launch-spawn-sentinel-1-1.env"
: >"$runtime/spawn-leaders-sentinel/spawn-sentinel"
seeded=$(cd "$runtime" && find . -mindepth 1 | LC_ALL=C sort)
status=0
"$@" || status=$?
after=$(cd "$runtime" && find . -mindepth 1 | LC_ALL=C sort)
if [ "$after" != "$seeded" ]; then
    echo "check-runtime-dir: the run changed $runtime (> written there, < removed from it):" >&2
    printf "%s\n" "$seeded" >"${TMPDIR:-/tmp}/crd-seeded.$$"
    printf "%s\n" "$after" | diff "${TMPDIR:-/tmp}/crd-seeded.$$" - | grep "^[<>]" >&2 || true
    rm -f "${TMPDIR:-/tmp}/crd-seeded.$$"
    exit 3
fi
echo "check-runtime-dir: $runtime as seeded after the run (exit $status)" >&2
exit "$status"
'
exec bwrap --dev-bind / / --perms 0700 --tmpfs "$runtime" \
    --setenv XDG_RUNTIME_DIR "$runtime" \
    sh -c "$inner" check-runtime-dir "$runtime" "$@"
