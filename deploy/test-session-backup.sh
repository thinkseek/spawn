#!/usr/bin/env bash
# Tests deploy/session-backup.sh (core (i), session backups) under a scratch
# root and a scratch HOME only (sync/cc-rules.md (a)): every home it is given
# and the backup root lie in this test's scratch, and guard_home refuses
# (exit 2, nothing run) before every run of the script otherwise.
#
#   bash deploy/test-session-backup.sh
#
# What it shows: the guard's refusals; a version is a whole copy with its
# manifest and counts (files and dirs per account); a shrink is said and
# keeps everything; thinning's first run on a root only prints; thinning
# never loses a file (path and content) that was ever copied, among them a
# file deleted from the source at hour 3, through any number of later runs;
# restore and the drill.
set -euo pipefail
export LC_ALL=C
repo=$(cd "$(dirname "$0")/.." && pwd)
script=$repo/deploy/session-backup.sh

real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
# Run as root (the pilot, rev-g): SPAWN_TEST_ACCOUNT owns every scratch home
# and SPAWN_TEST_BACKUP_USER the scratch root, both users other than root,
# as the system unit would have them. Run as an account, it is all of them.
if [ "$(id -u)" = 0 ]; then
  [ -n "${SPAWN_TEST_ACCOUNT-}" ] && [ -n "${SPAWN_TEST_BACKUP_USER-}" ] || {
    echo "test-session-backup: run as root, name SPAWN_TEST_ACCOUNT and SPAWN_TEST_BACKUP_USER; nothing ran" >&2
    exit 2
  }
  account_uid=$(id -u -- "$SPAWN_TEST_ACCOUNT") && bkp_uid=$(id -u -- "$SPAWN_TEST_BACKUP_USER") &&
    [ "$account_uid" != 0 ] && [ "$bkp_uid" != 0 ] && [ "$account_uid" != "$bkp_uid" ] || {
    echo "test-session-backup: SPAWN_TEST_ACCOUNT and SPAWN_TEST_BACKUP_USER must be two users, neither root; nothing ran" >&2
    exit 2
  }
  account_home=$(getent passwd "$account_uid" | cut -d: -f6)
  # A throwaway account only (rev-g's P2): as root the test deletes inside
  # homes it gives to the account, and a person's processes could swap a
  # scratch .claude for a symlink to a real one. No login, no .claude.
  case $(getent passwd "$account_uid" | cut -d: -f7) in */nologin | */false) ;; *)
    echo "test-session-backup: SPAWN_TEST_ACCOUNT must be a throwaway user with no login shell (useradd --system ... --shell /usr/sbin/nologin); nothing ran" >&2
    exit 2
    ;;
  esac
  [ ! -e "$account_home/.claude" ] || {
    echo "test-session-backup: SPAWN_TEST_ACCOUNT has a $account_home/.claude: a person's account; nothing ran" >&2
    exit 2
  }
else
  account_uid=$(id -u)
  bkp_uid=$account_uid
  account_home=$real_home
fi
scratch=$(mktemp -d "${TMPDIR:-/tmp}/spawn-session-backup-test.XXXXXX")
chmod 755 "$scratch"
# The scratch this test made, and only it.
cleanup() { case $scratch in "${TMPDIR:-/tmp}"/spawn-session-backup-test.??????) rm -rf -- "${scratch:?}" ;; esac; }
trap cleanup EXIT

export HOME=$scratch/home/owner
alice=$scratch/home/alice
bob=$scratch/home/bob
root=$scratch/backups/spawn-sessions
mkdir -p "$HOME" "$alice/.claude/projects/p1" "$bob/.claude/projects/p9" "$scratch/tmp"
chmod 1777 "$scratch/tmp"
if [ "$(id -u)" = 0 ]; then
  mkdir -p "$root" "$scratch/backups2/spawn-sessions" "$scratch/backups3/spawn-sessions"
  chown "$bkp_uid" "$root" "$scratch/backups2/spawn-sessions" "$scratch/backups3/spawn-sessions"
fi
# owned: every scratch home is the account's (as root; files the test writes
# are root's until then).
owned() { [ "$(id -u)" != 0 ] || chown -R -h "$account_uid" "$scratch/home"; }

# guard_home: HOME, the backup root and every home the script is given lie
# in the scratch and are neither in, nor hold, the real home.
guard_home() {
  local gh_scratch gh_real gh_path gh_paths
  [ -n "$scratch" ] && [ -n "$real_home" ] && [ -n "${HOME-}" ] || {
    echo "test-session-backup: refused: an empty home or scratch variable; nothing ran" >&2
    exit 2
  }
  for gh_path in "$account_home"; do
    case $(readlink -f -- "$scratch")/ in "$(readlink -f -- "$gh_path")"/*)
      echo "test-session-backup: refused: the scratch is in the home $gh_path; nothing ran" >&2
      exit 2
      ;;
    esac
  done
  gh_scratch=$(readlink -f -- "$scratch") && gh_real=$(readlink -f -- "$real_home") || {
    echo "test-session-backup: refused: a path that does not resolve; nothing ran" >&2
    exit 2
  }
  IFS=: read -r -a gh_paths <<<"$HOME:${SPAWN_SESSION_BACKUP_ROOT-}:${SPAWN_SESSION_HOMES-}"
  for gh_path in "${gh_paths[@]}"; do
    [ -n "$gh_path" ] || continue
    gh_path=$(readlink -m -- "$gh_path")
    case $gh_path/ in "$gh_scratch"/*) ;; *)
      echo "test-session-backup: refused: $gh_path is not inside the scratch $gh_scratch; nothing ran" >&2
      exit 2
      ;;
    esac
    case $gh_path/ in "$gh_real"/*)
      echo "test-session-backup: refused: $gh_path is in the real home; nothing ran" >&2
      exit 2
      ;;
    esac
    case $gh_real/ in "$gh_path"/*)
      echo "test-session-backup: refused: $gh_path holds the real home; nothing ran" >&2
      exit 2
      ;;
    esac
  done
}
guard_home

passed=0
failed=0
check() {
  local ck_label=$1
  shift
  if "$@"; then
    echo "ok   $ck_label"
    passed=$((passed + 1))
  else
    echo "FAIL $ck_label"
    failed=$((failed + 1))
  fi
}

# sb [VAR=value...] -- <args>: the script with the scratch root and homes
# (overridable), output in $out, status in $status.
out=$scratch/sb.out
sb() {
  local sb_env=()
  while [ $# -gt 0 ] && [ "$1" != -- ]; do
    sb_env+=("$1")
    shift
  done
  shift
  status=0
  (
    export SPAWN_SESSION_BACKUP_ROOT=$root SPAWN_SESSION_HOMES=$alice:$bob TMPDIR=$scratch/tmp
    [ "$(id -u)" != 0 ] || export SPAWN_SESSION_BACKUP_USER=$SPAWN_TEST_BACKUP_USER
    for sb_assignment in "${sb_env[@]}"; do export "${sb_assignment?}"; done
    guard_home
    owned
    case ${1-} in
    _*)
      # The backup user's own side, as it (run as root, through setpriv).
      if [ "$(id -u)" = 0 ]; then
        setpriv --reuid="$bkp_uid" --regid="$(getent passwd "$bkp_uid" | cut -d: -f4)" --clear-groups -- bash "$script" "$@"
      else
        bash "$script" "$@"
      fi
      ;;
    *) bash "$script" "$@" ;;
    esac
  ) >"$out" 2>&1 || status=$?
}
mkdir -p "$scratch/tmp"
at() { echo $((day0 + $1)); } # seconds after day 0's midnight, UTC
day0=$(date -u -d 2026-11-01 +%s)

echo "== the guard"
sb SPAWN_SESSION_BACKUP_ROOT= -- copy
check "no root: refused (status $status)" grep -q "SPAWN_SESSION_BACKUP_ROOT is not set" "$out"
sb SPAWN_SESSION_BACKUP_ROOT="$scratch/backups/elsewhere" -- copy
check "a root not named spawn-sessions: refused (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "ending in /spawn-sessions" "$2"' _ "$status" "$out"
sb SPAWN_SESSION_BACKUP_ROOT="$alice/b/spawn-sessions" -- copy
check "a root in a home it copies: refused (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "lies in the home" "$2"' _ "$status" "$out"
sb SPAWN_SESSION_HOMES="$root/x" -- copy
check "a home in the root: refused (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "holds the home" "$2"' _ "$status" "$out"
sb SPAWN_SESSION_HOMES="home/alice" -- copy
check "a relative home: refused (status $status)" [ "$status" -eq 2 ]
# As root the test made the root itself (owned by the backup user, above):
# a refused run made nothing in it, nor beside it.
check "nothing was made by a refused run" \
  bash -c '[ -z "$(find "$1" -mindepth 1 ! -path "$2" 2>/dev/null)" ] && [ -z "$(ls -A -- "$2" 2>/dev/null)" ] && [ ! -e "$3" ]' \
  _ "$scratch/backups" "$root" "$alice/b"
guard_status=0
(HOME=$real_home; guard_home) 2>"$scratch/guard.err" || guard_status=$?
check "the test's guard refuses the real home (status $guard_status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "is not inside the scratch" "$2"' _ "$guard_status" "$scratch/guard.err"
guard_status=0
(SPAWN_SESSION_HOMES=$real_home; guard_home) 2>"$scratch/guard.err" || guard_status=$?
check "and a real home given to the script (status $guard_status)" [ "$guard_status" -eq 2 ]

echo "== a version"
printf 'a1\n' >"$alice/.claude/projects/p1/a.jsonl"
printf 'b1\n' >"$alice/.claude/projects/p1/b.jsonl"
printf 'c-first-long-content\n' >"$alice/.claude/projects/p1/c.jsonl"
printf 'with space\n' >"$alice/.claude/projects/p1/name with space.md"
printf 'h1\n' >"$alice/.claude/history.jsonl"
mkdir -p "$alice/.claude/file-history/s1" "$alice/.claude/todos"
printf 'v1\n' >"$alice/.claude/file-history/s1/f@v1"
printf 'not copied\n' >"$alice/.claude/todos/t.json"
printf 'z\n' >"$bob/.claude/projects/p9/z.jsonl"
sb SPAWN_SESSION_BACKUP_NOW="$(at 0)" -- copy
v0=20261101T000000Z
check "copied (status $status)" [ "$status" -eq 0 ]
check "alice's version holds projects, history.jsonl and file-history" \
  bash -c 'cmp -s "$1/projects/p1/a.jsonl" "$2/.claude/projects/p1/a.jsonl" && cmp -s "$1/history.jsonl" "$2/.claude/history.jsonl" &&
    [ -f "$1/file-history/s1/f@v1" ] && [ -f "$1/projects/p1/name with space.md" ]' _ "$root/alice/$v0" "$alice"
check "and nothing else of .claude" [ ! -e "$root/alice/$v0/todos" ]
check "bob has his own" [ -f "$root/bob/$v0/projects/p9/z.jsonl" ]
check "its counts name the account's uid" \
  bash -c '[ "$(jq -r .uid "$1")" = "$2" ]' _ "$root/alice/$v0.counts" "$account_uid"
check "no work directory is left" bash -c '! compgen -G "$1/*/.incoming-*" >/dev/null' _ "$root"
check "its counts: 6 files, 4 dirs" \
  bash -c '[ "$(jq -r "[.files, .dirs] | @tsv" "$1")" = "6	4" ]' _ "$root/alice/$v0.counts"
check "its manifest has each file's sha256" \
  bash -c 'while read -r sha size mtime path; do [ "$(sha256sum <"$1/$path" | cut -d" " -f1)" = "$sha" ] || exit 1; done <"$1.manifest"
    [ "$(wc -l <"$1.manifest")" -eq 6 ]' _ "$root/alice/$v0"
check "no .partial is left" bash -c '! compgen -G "$1/*/*.partial" >/dev/null' _ "$root"
check "the account directories are 0700, the root 0711" \
  bash -c '[ "$(stat -c %a "$1")" = 711 ] && [ "$(stat -c %a "$1/alice")" = 700 ] && [ "$(stat -c %a "$1/alice/$2")" = 700 ]' _ "$root" "$v0"
check "status.json: 0644, both accounts, the estimate and the free bytes" \
  bash -c '[ "$(stat -c %a "$1/status.json")" = 644 ] &&
    jq -e "(.accounts | map(.account) == [\"alice\", \"bob\"]) and .estimated_bytes > 0 and .free_bytes > 0" "$1/status.json" >/dev/null' _ "$root"
sb SPAWN_SESSION_BACKUP_NOW="$(at 0)" -- copy
check "the same stamp twice: refused (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "exists already" "$2"' _ "$status" "$out"

echo "== the next version: an append costs what was appended"
printf 'a2-appended\n' >>"$alice/.claude/projects/p1/a.jsonl"
sb SPAWN_SESSION_BACKUP_NOW="$(at 900)" -- copy
check "new_bytes is the 12 appended bytes (status $status)" \
  bash -c '[ "$(jq -r .new_bytes "$1")" = 12 ]' _ "$root/alice/20261101T001500Z.counts"
check "an unchanged file keeps its sha256" \
  bash -c '[ "$(grep " p1/b.jsonl$" "$1/20261101T000000Z.manifest" | cut -d" " -f1)" = "$(grep " p1/b.jsonl$" "$1/20261101T001500Z.manifest" | cut -d" " -f1)" ]' _ "$root/alice"

# Day 1 (2026-11-01) in versions every 15 minutes, as the timer makes them.
# b.jsonl is deleted from the source at hour 3; c.jsonl is rewritten (not
# appended to) at hour 5; a.jsonl grows all day.
echo "== a day of versions"
for ((t = 1800; t < 24 * 3600; t += 900)); do
  if [ "$t" -eq $((3 * 3600)) ]; then rm -- "$alice/.claude/projects/p1/b.jsonl"; fi
  if [ "$t" -eq $((5 * 3600)) ]; then printf 'c-second\n' >"$alice/.claude/projects/p1/c.jsonl"; fi
  printf 'a at %s\n' "$t" >>"$alice/.claude/projects/p1/a.jsonl"
  sb SPAWN_SESSION_BACKUP_NOW="$(at "$t")" -- copy
  [ "$status" -eq 0 ] || {
    cat "$out"
    exit 1
  }
done
# Days 2 and 3, a few versions each; then three in the last 48 hours (now is
# day 6's midnight).
for t in $((24 * 3600)) $((30 * 3600)) $((36 * 3600)) $((47 * 3600)) $((48 * 3600)) $((60 * 3600)) \
  $((71 * 3600)) $((5 * 86400 - 7200)) $((5 * 86400 - 3600)) $((5 * 86400 - 2700)); do
  printf 'a at %s\n' "$t" >>"$alice/.claude/projects/p1/a.jsonl"
  sb SPAWN_SESSION_BACKUP_NOW="$(at "$t")" -- copy
done
now=$((5 * 86400))
all=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" -printf '%f\n' | sort)
count_all=$(wc -l <<<"$all")
check "$count_all versions of alice" [ "$count_all" -eq 106 ]
# Every (path, content) ever copied: the manifests before any thinning.
had_blocks=no
[ ! -e "$root/alice/20261101T060000Z.blocks" ] || had_blocks=yes
mkdir "$scratch/manifests"
cp -- "$root"/alice/*.manifest "$scratch/manifests/"

# lost: every file of every manifest ever written that no kept version holds
# (at its path, with its content or that content as its beginning).
lost() {
  local lo_m lo_sha lo_size lo_mtime lo_path lo_v lo_found
  for lo_m in "$scratch"/manifests/*.manifest; do
    while read -r lo_sha lo_size lo_mtime lo_path; do
      lo_found=0
      for lo_v in "$root"/alice/2*Z; do
        [ -f "$lo_v/$lo_path" ] || continue
        if [ "$(head -c "$lo_size" <"$lo_v/$lo_path" | sha256sum | cut -d' ' -f1)" = "$lo_sha" ]; then
          lo_found=1
          break
        fi
      done
      [ "$lo_found" = 1 ] || echo "$(basename "$lo_m") $lo_path"
    done <"$lo_m"
  done
}

echo "== thinning: a first run the budget cuts is not the print-only run (rev-j's 229 R0 F1)"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" SPAWN_SESSION_THIN_BUDGET=0 -- thin
check "a first thin with a budget of 0 prints only, judges nothing and leaves no marker (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "is print-only (rule (c))" "$2" && ! grep -qE ": (would remove|kept|removed) " "$2" && grep -q "not judged yet (budget 0s)" "$2" && [ ! -e "$3/.thin-dry-run-done" ]' _ "$status" "$out" "$root"
check "and removed none" [ "$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)" -eq "$count_all" ]
echo "== thinning: the first run on a root only prints"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin
cp -- "$out" "$scratch/thin-first.out"
check "says it is print-only (status $status)" grep -q "is print-only (rule (c))" "$out"
check "would remove versions" grep -q "would remove 20261101T" "$out"
check "removed none" [ "$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)" -eq "$count_all" ]
check "left the marker" [ -e "$root/.thin-dry-run-done" ]
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin --dry-run
check "--dry-run after it removes none either" \
  bash -c '[ "$(find "$1" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)" -eq "$2" ] && ! grep -q "removed " "$3"' _ "$root/alice" "$count_all" "$out"
check "and says it removes nothing, not that a next one removes" \
  bash -c 'grep -q "thin --dry-run: prints what it would remove, removes nothing" "$1" && ! grep -q "the next one" "$1"' _ "$out"

echo "== thinning keeps its verdicts (objective 229)"
# verdicts <file>: the lines of a thin's verdicts, in its order.
verdicts() { grep -E '^session-backup: alice: (would remove|kept) ' "$1" || true; }
v_first=$(verdicts "$scratch/thin-first.out")
check "the first run judged pairs, among them a version it would remove ($(wc -l <<<"$v_first") verdicts)" \
  bash -c 'grep -q ": would remove 20261101T070000Z " <<<"$1" && grep -q ": would remove 20261101T060000Z " <<<"$1"' _ "$v_first"
check "and kept them, one line a pair, under the account" \
  bash -c '[ "$(wc -l <"$1/alice/.thin-judged")" -eq "$(wc -l <<<"$2")" ] && [ "$(stat -c %a "$1/alice/.thin-judged")" = 600 ]' _ "$root" "$v_first"
check "the --dry-run after it says the same verdicts" bash -c '[ "$1" = "$2" ]' _ "$v_first" "$(verdicts "$out")"
# A kept verdict is used without reading the versions: with every file of
# 20261101T060000Z and of the version after it unreadable, a --dry-run still
# says it would remove 20261101T060000Z (a fresh judgment could not read
# them, and would keep it).
unreadable=$(find "$root/alice/20261101T060000Z" "$root/alice/20261101T061500Z" -type f)
while read -r f; do chmod 000 -- "$f"; done <<<"$unreadable"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin --dry-run
check "a --dry-run reads no version it has a verdict for: the same verdicts with both versions unreadable (status $status)" \
  bash -c '[ "$1" -eq 0 ] && [ "$2" = "$3" ]' _ "$status" "$v_first" "$(verdicts "$out")"
# A line is used only while both manifests hash as it says: with the
# line's sha256 for the next version's manifest made wrong, the pair is
# judged again, and (its files still unreadable) kept.
[ ! -e "$root/alice/.thin-judged" ] || sed -i 's/^\(20261101T060000Z 20261101T061500Z [a-z]* [0-9a-f]* \)[0-9a-f]*$/\10000000000000000000000000000000000000000000000000000000000000000/' "$root/alice/.thin-judged"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin --dry-run
while read -r f; do chmod 400 -- "$f"; done <<<"$unreadable"
check "a line whose manifest sha256 no longer matches is not used: 20261101T060000Z judged again, and kept (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "alice: kept 20261101T060000Z: " "$2" && ! grep -q "alice: would remove 20261101T060000Z " "$2"' _ "$status" "$out"
# The budget: with none, nothing new is judged and the run says so; with
# one, the next run judges the rest, and says what a full run says.
# (moved only if there: a missing file is the check above failing, not a harness stop)
[ ! -e "$root/alice/.thin-judged" ] || mv -- "$root/alice/.thin-judged" "$scratch/thin-judged.saved"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" SPAWN_SESSION_THIN_BUDGET=0 -- thin --dry-run
# (bob's verdicts are still kept, so bob's lines are said; alice's are not.)
check "a budget of 0 judges nothing new and says how many wait (status $status; $(grep -o "thin: .*" "$out"))" \
  bash -c '[ "$1" -eq 0 ] && ! grep -qE "^session-backup: alice: (would remove|kept) " "$2" && grep -qE "^session-backup: bob: would remove " "$2" && grep -q "thin: $3 version(s) not judged yet (budget 0s); the next run goes on" "$2"' _ "$status" "$out" "$(wc -l <<<"$v_first")"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin --dry-run
check "the next run, with the default budget, judges them all: the same verdicts, none waiting" \
  bash -c '[ "$1" = "$2" ] && ! grep -q "not judged yet" "$3"' _ "$v_first" "$(verdicts "$out")" "$out"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" SPAWN_SESSION_THIN_BUDGET=soon -- thin --dry-run
check "a budget that is no number is refused (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "SPAWN_SESSION_THIN_BUDGET must be a number of seconds" "$2"' _ "$status" "$out"
# A removal judges at the moment of removal: 20261101T070000Z has a kept
# "held" verdict, but its next version's a.jsonl is changed now (same size,
# its first byte), so the version no longer holds in the next one. The
# thinning below must keep it, and say so.
nf=$root/alice/20261101T071500Z/projects/p1/a.jsonl
nf_byte=$(head -c 1 "$nf")
chmod 600 -- "$nf"
printf 'X' | dd of="$nf" bs=1 seek=0 count=1 conv=notrunc status=none
chmod 400 -- "$nf"
echo "== thinning"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin
kept=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" -printf '%f\n' | sort)
check "removed versions (status $status; $(wc -l <<<"$kept") kept of $count_all)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(wc -l <<<"$2")" -lt "$3" ] && grep -q "removed 20261101T" "$4"' _ "$status" "$kept" "$count_all" "$out"
check "nothing ever copied is lost" bash -c '[ -z "$1" ]' _ "$(lost)"
check "the last version before b.jsonl's deletion at hour 3 is kept" grep -qx 20261101T024500Z <<<"$kept"
check "a removal does not trust a kept verdict: 20261101T070000Z, whose next changed, is kept and said so" \
  bash -c 'grep -qx 20261101T070000Z <<<"$1" && grep -q "alice: kept 20261101T070000Z: a file of it is in no later version" "$2"' _ "$kept" "$out"
check "after the removals, every kept line names two versions that are still there" \
  bash -c 'while read -r v n rest; do [ -d "$1/$v" ] && [ -d "$1/$n" ] || exit 1; done <"$1/.thin-judged"; [ -s "$1/.thin-judged" ]' _ "$root/alice"
# The byte put back: the version is its manifest's again.
chmod 600 -- "$nf"
printf '%s' "$nf_byte" | dd of="$nf" bs=1 seek=0 count=1 conv=notrunc status=none
chmod 400 -- "$nf"
check "and holds b.jsonl" [ -f "$root/alice/20261101T024500Z/projects/p1/b.jsonl" ]
check "the last version before c.jsonl's rewrite at hour 5 is kept" grep -qx 20261101T044500Z <<<"$kept"
check "and those after the rewrite and in the 48 hours' days, every file of which is in the next, are gone" \
  bash -c '! grep -qx 20261103T120000Z <<<"$1"' _ "$kept"
check "a version every file of which is in the next is gone" bash -c '! grep -qx 20261101T060000Z <<<"$1"' _ "$kept"
check "its manifest and counts are gone with it" \
  bash -c '[ ! -e "$1.manifest" ] && [ ! -e "$1.counts" ]' _ "$root/alice/20261101T060000Z"
check "and its .blocks (it had one: $had_blocks)" \
  bash -c '[ "$2" = yes ] && [ ! -e "$1.blocks" ]' _ "$root/alice/20261101T060000Z" "$had_blocks"
check "each day's last version is kept" \
  bash -c 'grep -qx 20261101T234500Z <<<"$1" && grep -qx 20261102T230000Z <<<"$1" && grep -qx 20261103T230000Z <<<"$1"' _ "$kept"
check "every version of the last 48 hours is kept" \
  bash -c 'for v in 20261105T220000Z 20261105T230000Z 20261105T231500Z; do grep -qx "$v" <<<"$1" || exit 1; done' _ "$kept"
check "status.json follows the thinning" \
  bash -c 'jq -e ".accounts[0].version == \"20261105T231500Z\"" "$1/status.json" >/dev/null' _ "$root"

echo "== thinning again, months later: the file deleted at hour 3 survives"
for later in $((30 * 86400)) $((90 * 86400)) $((400 * 86400)); do
  sb SPAWN_SESSION_BACKUP_NOW="$(at "$later")" -- thin
done
check "b.jsonl is still in a kept version a year later (status $status)" [ -f "$root/alice/20261101T024500Z/projects/p1/b.jsonl" ]
check "and still nothing ever copied is lost" bash -c '[ -z "$1" ]' _ "$(lost)"

echo "== a shrink"
rm -rf -- "${alice:?}/.claude/projects"
mkdir -p "$alice/.claude/projects"
before=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)
sb SPAWN_SESSION_BACKUP_NOW="$(at $((5 * 86400 + 900)))" -- copy
check "copied, saying SHRANK (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "SHRANK alice: 2 files now, 5 in 20261105T231500Z" "$2"' _ "$status" "$out"
check "recorded in its counts and status.json" \
  bash -c 'jq -e ".shrank" "$1/alice/20261106T001500Z.counts" >/dev/null &&
    jq -e "[.accounts[] | select(.account == \"alice\")][0].shrank" "$1/status.json" >/dev/null' _ "$root"
check "every earlier version is kept" [ "$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)" -eq $((before + 1)) ]

echo "== refused homes (rev-g's F1)"
carol=$scratch/home/carol
dave=$scratch/home/dave
mkdir -p "$carol/elsewhere/.claude/projects/p" "$dave/.claude" "$dave/real-projects/p"
printf 'c\n' >"$carol/elsewhere/.claude/projects/p/c.jsonl"
ln -s elsewhere/.claude "$carol/.claude"
printf 'd\n' >"$dave/real-projects/p/d.jsonl"
ln -s ../real-projects "$dave/.claude/projects"
sb SPAWN_SESSION_HOMES="$carol:$dave:$bob" SPAWN_SESSION_BACKUP_NOW="$(at $((5 * 86400 + 1800)))" -- copy
check "a symlinked .claude is refused (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "REFUSED carol: .*/carol/.claude is a symlink" "$2" && [ ! -e "$3/carol" ]' _ "$status" "$out" "$root"
check "a symlinked item in .claude is refused" \
  bash -c 'grep -q "REFUSED dave: .*/dave/.claude/projects is a symlink" "$1" && [ ! -e "$2/dave" ]' _ "$out" "$root"
check "and the other homes are copied" [ -d "$root/bob/20261106T003000Z" ]
check "status.json is written all the same" \
  bash -c 'jq -e ".at == $2" "$1/status.json" >/dev/null' _ "$root" "$(at $((5 * 86400 + 1800)))"

echo "== restore"
# The targets lie where the account can make a directory (the scratch's
# 1777 tmp): the script makes the target as the account, and as root the
# scratch itself is root's 0755.
rt=$scratch/tmp
sb -- restore alice 20261101T024500Z "$rt/restored"
check "into a scratch directory, checked against its manifest (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "restored alice 20261101T024500Z" "$2" && [ -f "$3/projects/p1/b.jsonl" ]' _ "$status" "$out" "$rt/restored"
sb -- restore alice 20261101T024500Z "$rt/restored"
check "not into a directory that is not empty (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "is not an empty directory" "$2" && [ -f "$3/projects/p1/b.jsonl" ]' _ "$status" "$out" "$rt/restored"
sb -- restore alice 20261101T024500Z "$alice/.claude/projects/r"
check "never into the live sessions (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "is in the live sessions" "$2" && [ ! -e "$3" ]' _ "$status" "$out" "$alice/.claude/projects/r"
sb -- restore alice ../bob "$rt/r2"
check "not a name that is no version (status $status)" grep -q "not a version" "$out"

cp -- "$root/alice/20261101T024500Z.counts" "$scratch/counts.kept"
jq -c '.uid = 0' "$scratch/counts.kept" >"$root/alice/20261101T024500Z.counts"
sb -- restore alice 20261101T024500Z "$rt/restored-as-root"
check "never as another account than the one copied (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "uid 0" "$2" && [ ! -e "$3" ]' _ "$status" "$out" "$rt/restored-as-root"
jq -c '.uid = 4242' "$scratch/counts.kept" >"$root/alice/20261101T024500Z.counts"
sb -- restore alice 20261101T024500Z "$rt/restored-as-other"
check "nor as a uid that does not own the account's .claude (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "names uid 4242, and alice.s .claude is uid" "$2" && [ ! -e "$3" ]' _ "$status" "$out" "$rt/restored-as-other"
cp -- "$scratch/counts.kept" "$root/alice/20261101T024500Z.counts"

echo "== the drill"
sb -- drill
check "passes for each account (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "DRILL PASSED .* alice 20261106T001500Z" "$2" && grep -q "DRILL PASSED .* bob " "$2"' _ "$status" "$out"
check "removes its scratch" bash -c '! compgen -G "$1/spawn-session-drill.*" >/dev/null' _ "$scratch/tmp"
printf 'damaged\n' >"$root/bob/20261106T003000Z/projects/p9/z.jsonl"
sb -- drill
check "fails on a damaged version (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "DRILL FAILED .* bob 20261106T003000Z.*does not match its manifest.*projects/p9/z.jsonl" "$2"' _ "$status" "$out"
printf 'z\n' >"$root/bob/20261106T003000Z/projects/p9/z.jsonl"

echo "== blocks beside each version (objective 147)"
# A root of its own: frank's big file spans blocks, and grows.
frank=$scratch/home/frank
root3=$scratch/backups3/spawn-sessions
mkdir -p "$frank/.claude/projects/p"
head -c 200000 /dev/urandom >"$frank/.claude/projects/p/big.bin"
printf 'small\n' >"$frank/.claude/projects/p/a.jsonl"
s3() { sb SPAWN_SESSION_BACKUP_ROOT="$root3" SPAWN_SESSION_HOMES="$frank" "$@"; }
s3 SPAWN_SESSION_BACKUP_NOW="$(at 0)" -- copy
f0=20261101T000000Z
check "a version has its .blocks, 0400, its files read whole (status $status)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(stat -c %a "$2.blocks")" = 400 ] && grep -q "frank: blocks $3: 2 files, 5 blocks, 5 new (.*), 200006 bytes read" "$4"' \
  _ "$status" "$root3/frank/$f0" "$f0" "$out"
check "every block is 0400, named by its plaintext's sha256" \
  bash -c 'cd "$1" && n=0 && for b in */*.zst; do [ "$(stat -c %a "$b")" = 400 ] || exit 1
      [ "$(zstd -d -q -c -- "$b" | sha256sum | cut -d" " -f1).zst" = "$(basename "$b")" ] || exit 1; n=$((n + 1)); done; [ "$n" -eq 5 ]' \
  _ "$root3/frank/blocks"
check "its index agrees with its manifest" \
  bash -c 'diff <(head -n -1 "$1.blocks" | jq -r "[.sha256, (.size | tostring), .path] | join(\" \")") <(awk "{print \$1, \$2, \$4}" "$1.manifest")' _ "$root3/frank/$f0"
head -c 70000 /dev/urandom >>"$frank/.claude/projects/p/big.bin"
s3 SPAWN_SESSION_BACKUP_NOW="$(at 900)" -- copy
f1=20261101T001500Z
big() { head -n -1 "$1.blocks" | jq -c 'select(.path == "projects/p/big.bin") | .blocks'; }
check "a grown file reads and writes only its tail, an unchanged one nothing: 2 new blocks, 73392 bytes read, its first 3 kept (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "frank: blocks $2: 2 files, 6 blocks, 2 new (.*), 73392 bytes read" "$3" &&
    [ "$(jq -c ".[:3]" <<<"$4")" = "$(jq -c ".[:3]" <<<"$5")" ] && [ "$(jq length <<<"$5")" -eq 5 ]' \
  _ "$status" "$f1" "$out" "$(big "$root3/frank/$f0")" "$(big "$root3/frank/$f1")"
s3 -- _check frank
check "check passes on a whole store (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "check frank: 2 indexes, 3 distinct files, 7 blocks named" "$2"' _ "$status" "$out"
s3 -- restore --from-blocks frank "$f1" "$rt/from-blocks"
fb_status=$status
s3 -- restore frank "$f1" "$rt/from-dir"
check "restore --from-blocks: checked against its manifest, byte for byte the directory's (status $fb_status)" \
  bash -c '[ "$1" -eq 0 ] && [ "$2" -eq 0 ] && diff -r -q -- "$3" "$4" && [ -s "$3/projects/p/big.bin" ]' \
  _ "$fb_status" "$status" "$rt/from-blocks" "$rt/from-dir"
s3 -- drill
check "the drill rebuilds the newest from its blocks and checks them (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "DRILL PASSED .* frank $2: .*rebuilt from its blocks byte for byte; check frank: " "$3"' _ "$status" "$f1" "$out"

# damage <what>: one kind of damage to frank's store, then check; the store
# is put back after.
idx=$root3/frank/$f1.blocks
b0=$(jq -r '.[0]' <<<"$(big "$root3/frank/$f1")")
b4=$(jq -r '.[4]' <<<"$(big "$root3/frank/$f1")")
blk() { printf '%s/frank/blocks/%s/%s.zst' "$root3" "${1:0:2}" "$1"; }
cp -p -- "$idx" "$scratch/idx.kept"
cp -p -- "$(blk "$b0")" "$scratch/b0.kept"
cp -p -- "$(blk "$b4")" "$scratch/b4.kept"
put_back() {
  chmod 600 "$idx" "$(blk "$b0")" "$(blk "$b4")" 2>/dev/null || true
  cp -- "$scratch/idx.kept" "$idx" && cp -- "$scratch/b0.kept" "$(blk "$b0")" && cp -- "$scratch/b4.kept" "$(blk "$b4")"
  chmod 400 "$idx" "$(blk "$b0")" "$(blk "$b4")"
}
edit_index() { # <jq filter on big.bin's line>
  chmod 600 "$idx"
  { head -n -1 "$scratch/idx.kept" | jq -c "if .path == \"projects/p/big.bin\" then $1 else . end"; tail -n 1 "$scratch/idx.kept"; } >"$idx"
}
chmod 600 "$(blk "$b0")"
head -c 65536 /dev/urandom | zstd -3 -q -c >"$(blk "$b0")"
s3 -- _check frank
check "check fails on a block that does not hash to its name (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: .*block $2 does not hash to its name" "$3"' _ "$status" "$b0" "$out"
s3 -- restore --from-blocks frank "$f1" "$rt/from-bad-block"
check "and restore --from-blocks refuses it (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "does not hash to its name" "$2"' _ "$status" "$out"
s3 -- drill
check "and the drill fails (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "DRILL FAILED .* frank $2: from its blocks" "$3"' _ "$status" "$f1" "$out"
put_back
edit_index '.blocks = [.blocks[1], .blocks[0]] + .blocks[2:]'
s3 -- _check frank
check "check fails on a block list in the wrong order, every block whole (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2 projects/p/big.bin: its blocks do not rebuild its sha256" "$3"' _ "$status" "$f1" "$out"
s3 -- restore --from-blocks frank "$f1" "$rt/from-bad-list"
check "and restore --from-blocks refuses it (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "projects/p/big.bin: its blocks do not rebuild its sha256" "$2"' _ "$status" "$out"
put_back
edit_index '.sha256 = ("0" * 64)'
s3 -- _check frank
check "check fails on an index that disagrees with its manifest (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks disagrees with its manifest: projects/p/big.bin" "$3"' _ "$status" "$f1" "$out"
put_back
# A whole file's line gone, the summary made to count what is left (A61).
chmod 600 "$idx"
{ head -n -1 "$scratch/idx.kept" | jq -c 'select(.path != "projects/p/big.bin")'
  tail -n 1 "$scratch/idx.kept" | jq -c --argjson n "$(head -n -1 "$scratch/idx.kept" | jq -s '[.[] | select(.path != "projects/p/big.bin")] | {files: length, bytes: (map(.size) | add), blocks: (map(.blocks | length) | add)}')" '. + $n'; } >"$idx"
s3 -- _check frank
check "check fails on an index missing a whole file, its summary made to agree (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks disagrees with its manifest: projects/p/big.bin" "$3"' _ "$status" "$f1" "$out"
put_back
# A file's line twice.
chmod 600 "$idx"
{ head -n -1 "$scratch/idx.kept"; head -n 1 "$scratch/idx.kept"; tail -n 1 "$scratch/idx.kept"; } >"$idx"
s3 -- _check frank
check "check fails on an index with a file's line twice (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks its file lines are not sorted and unique" "$3"' _ "$status" "$f1" "$out"
put_back
# A summary whose bytes are not its files' (A61: totals reconciled).
chmod 600 "$idx"
{ head -n -1 "$scratch/idx.kept"; tail -n 1 "$scratch/idx.kept" | jq -c '.bytes += 1'; } >"$idx"
s3 -- _check frank
check "check fails on a summary whose bytes are not its files' (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks its summary says" "$3"' _ "$status" "$f1" "$out"
put_back
# A copy killed before it published its index (A63): the version is its
# directory, its index a .partial that check reports and nothing reads.
mv -- "$idx" "$idx.partial"
s3 -- _check frank
check "an index left unpublished is reported (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks.partial is left" "$3"' _ "$status" "$f1" "$out"
s3 -- restore frank "$f1" "$rt/unpublished"
check "and its version still restores from its directory (status $status)" [ "$status" -eq 0 ]
mv -- "$idx.partial" "$idx"
# A copy that fails after its index is written and before its version is
# committed (the test's barrier): no index names the uncommitted version.
printf 'barrier\n' >>"$frank/.claude/projects/p/a.jsonl"
s3 SPAWN_SESSION_BACKUP_TEST_FAIL_AT=before-commit SPAWN_SESSION_BACKUP_NOW="$(at 1500)" -- copy
check "a copy failing before its commit leaves no index of the version (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "failed before its commit" "$2" && [ ! -e "$3.blocks" ] && [ ! -e "$3.blocks.partial" ] && [ ! -d "$3" ]' \
  _ "$status" "$out" "$root3/frank/20261101T002500Z"
# Its manifest and counts are left, as before 147 (a copy that fails after
# writing them; check reports them): the test removes them.
s3 -- _check frank
check "check reports the failed copy's manifest, whose content is nowhere (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: 20261101T002500Z.manifest has neither a .blocks nor a directory" "$2"' _ "$status" "$out"
rm -f -- "$root3/frank/20261101T002500Z.manifest" "$root3/frank/20261101T002500Z.counts"
# An index for a version that was never committed.
{ head -n -1 "$idx"; tail -n 1 "$idx" | jq -c '.version = "20261101T235900Z"'; } >"$root3/frank/20261101T235900Z.blocks"
s3 -- _check frank
check "an index naming no committed version is reported (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: 20261101T235900Z.blocks names no committed version" "$2"' _ "$status" "$out"
chmod 600 "$root3/frank/20261101T235900Z.blocks"
rm -f -- "$root3/frank/20261101T235900Z.blocks"
chmod 600 "$idx"
head -n -1 "$scratch/idx.kept" >"$idx"
s3 -- _check frank
check "check fails on an index cut short (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: $2.blocks its last line is not the summary" "$3"' _ "$status" "$f1" "$out"
put_back
# The last block, short, then 70000 bytes more in its frame.
chmod 600 "$(blk "$b4")"
{ zstd -d -q -c -- "$scratch/b4.kept"; head -c 70000 /dev/urandom; } | zstd -3 -q -c >"$(blk "$b4")"
s3 -- _check frank
check "check fails on a block that decompresses beyond its size (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: .*decompress beyond" "$2"' _ "$status" "$out"
# The file's own bytes come first, so a reader that stops at its size reads
# them whole (rev-e's F1, cc-astra's A66): restore refuses it all the same.
s3 -- restore --from-blocks frank "$f1" "$rt/from-overlong"
check "and restore --from-blocks refuses it, though the file's bytes come first (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "decompress beyond" "$2"' _ "$status" "$out"
put_back
# The first block's frame, whole, then bytes that are no frame: zstd gives
# the right bytes, goes on to the next block, and fails at its exit.
chmod 600 "$(blk "$b0")"
{ cat -- "$scratch/b0.kept"; printf 'not a frame'; } >"$(blk "$b0")"
s3 -- _check frank
check "check fails on a block zstd fails on after its bytes (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: .*zstd failed on the blocks" "$2"' _ "$status" "$out"
s3 -- restore --from-blocks frank "$f1" "$rt/from-zstd-failed"
check "and restore --from-blocks refuses it (status $status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "zstd failed on the blocks" "$2"' _ "$status" "$out"
put_back
# A manifest naming a path twice, the later line the right one.
mf=$root3/frank/$f1.manifest
cp -p -- "$mf" "$scratch/mf.kept"
chmod 600 "$mf"
{ sed -n 1p "$scratch/mf.kept" | awk '{ $1 = "0000000000000000000000000000000000000000000000000000000000000000"; print }'; cat -- "$scratch/mf.kept"; } >"$mf"
s3 -- _check frank
check "check fails on a manifest naming a path twice (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: .*the manifest names .* twice" "$2"' _ "$status" "$out"
cp -- "$scratch/mf.kept" "$mf"
chmod "$(stat -c %a "$scratch/mf.kept")" "$mf"
mv -- "$(blk "$b4")" "$scratch/b4.moved"
s3 -- _check frank
check "check fails on a missing block (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "CHECK FAILED frank: .*block $2 is missing" "$3"' _ "$status" "$b4" "$out"
mv -- "$scratch/b4.moved" "$(blk "$b4")"
s3 -- _check frank
check "and passes once each is put back (status $status)" [ "$status" -eq 0 ]

# A block write that fails: the version is kept, without blocks; the next
# one writes its files whole.
# Every directory of the blocks read-only, so no new block can be written
# (whatever its first two hex digits).
find "$root3/frank/blocks" -type d -exec chmod 500 {} +
printf 'more\n' >>"$frank/.claude/projects/p/a.jsonl"
s3 SPAWN_SESSION_BACKUP_NOW="$(at 1800)" -- copy
find "$root3/frank/blocks" -type d -exec chmod 700 {} +
f2=20261101T003000Z
check "a block write that fails says so and keeps the version, without blocks (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "BLOCKS FAILED frank $2" "$3" && [ -d "$4/$2" ] && [ ! -e "$4/$2.blocks" ] && [ ! -e "$4/$2.blocks.partial" ]' \
  _ "$status" "$f2" "$out" "$root3/frank"
printf 'more again\n' >>"$frank/.claude/projects/p/a.jsonl"
s3 SPAWN_SESSION_BACKUP_NOW="$(at 2700)" -- copy
s3 -- _check frank
check "the next version has its blocks again, and check passes (status $status)" \
  bash -c '[ "$1" -eq 0 ] && [ -e "$2.blocks" ]' _ "$status" "$root3/frank/20261101T004500Z"

echo "== the backup lock (objective 147)"
# hold_lock <-x|-s>: another holder of the root's lock, until release_lock.
hold_lock() {
  bash -c 'exec 9<"$2" && flock "$1" 9 && exec sleep 120' _ "$1" "$root3" &
  holder=$!
  for _ in $(seq 100); do
    flock -n -x "$root3" true 2>/dev/null || return 0
    sleep 0.1
  done
  echo "test-session-backup: the lock was not taken" >&2
  exit 2
}
release_lock() {
  kill "$holder" 2>/dev/null || true
  wait "$holder" 2>/dev/null || true
}
versions3() { find "$root3/frank" -mindepth 1 -maxdepth 1 -type d -name '2*Z' | wc -l; }
status_at=$(jq .at "$root3/status.json")
n3=$(versions3)
hold_lock -x
printf 'locked out\n' >>"$frank/.claude/projects/p/a.jsonl"
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 SPAWN_SESSION_BACKUP_NOW="$(at 3600)" -- copy
check "a copy that cannot have the lock skips, says so, copies nothing (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "SKIPPED copy: the backup lock on .* is held (waited 1 s)" "$2" && [ "$3" -eq "$4" ]' \
  _ "$status" "$out" "$n3" "$(versions3)"
check "and records the skip in status.json, its at left as the last copy's" \
  bash -c 'jq -e ".last_skip.what == \"copy\" and .last_skip.because == \"locked\" and .at == $2" "$1/status.json" >/dev/null' \
  _ "$root3" "$status_at"
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 SPAWN_SESSION_BACKUP_NOW="$(at 3700)" -- thin --dry-run
check "a thin skips the same way (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "SKIPPED thin" "$2" && jq -e ".last_skip.what == \"thin\"" "$3/status.json" >/dev/null' \
  _ "$status" "$out" "$root3"
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 -- restore frank "$f1" "$rt/locked-out"
check "a restore that cannot have the lock fails, restoring nothing (status $status)" \
  bash -c '[ "$1" -eq 2 ] && grep -q "nothing restored" "$2" && [ ! -e "$3" ]' _ "$status" "$out" "$rt/locked-out"
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 -- drill
check "a drill that cannot have the lock fails (status $status)" \
  bash -c '[ "$1" -eq 1 ] && grep -q "DRILL FAILED .*: the backup lock on .* is held" "$2"' _ "$status" "$out"
release_lock
hold_lock -s
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 -- restore frank "$f1" "$rt/shared"
check "a restore runs beside another reader (status $status)" [ "$status" -eq 0 ]
s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 SPAWN_SESSION_BACKUP_NOW="$(at 3800)" -- copy
check "and a copy waits for every reader (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "SKIPPED copy" "$2"' _ "$status" "$out"
release_lock
s3 SPAWN_SESSION_BACKUP_NOW="$(at 3900)" -- copy
check "once the lock is free, the copy runs, and the last skip is still said (status $status)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(find "$2/frank" -mindepth 1 -maxdepth 1 -type d -name "2*Z" | wc -l)" -eq $(($3 + 1)) ] &&
    jq -e ".last_skip.what == \"copy\" and .at == $4" "$2/status.json" >/dev/null' _ "$status" "$root3" "$n3" "$(at 3900)"

echo "== status writes under the status lock (cc-astra's A65)"
# A copy holds the status lock (the test's barrier, after it has read the
# status before); a copy that cannot have the backup lock skips meanwhile.
# The skip waits for the status lock, so the status keeps both: the copy's
# at and the skip.
hold=$scratch/status-hold
mkdir -p "$hold" && chmod 777 "$hold"
printf 'status race\n' >>"$frank/.claude/projects/p/a.jsonl"
(
  out=$scratch/a65-copy.out
  s3 SPAWN_SESSION_BACKUP_TEST_STATUS_HOLD="$hold" SPAWN_SESSION_BACKUP_NOW="$(at 4100)" -- copy
  echo "$status" >"$scratch/a65-copy.status"
) &
a65_copy=$!
for _ in $(seq 300); do [ -e "$hold/in" ] && break; sleep 0.1; done
(
  out=$scratch/a65-skip.out
  s3 SPAWN_SESSION_BACKUP_LOCK_WAIT=1 SPAWN_SESSION_BACKUP_NOW="$(at 4150)" -- copy
  echo "$status" >"$scratch/a65-skip.status"
) &
a65_skip=$!
for _ in $(seq 300); do grep -qs SKIPPED "$scratch/a65-skip.out" && break; sleep 0.1; done
sleep 1
check "a skip beside a status publication waits for it" \
  bash -c '[ -e "$1/in" ] && grep -q SKIPPED "$2" && [ ! -e "$3" ]' _ "$hold" "$scratch/a65-skip.out" "$scratch/a65-skip.status"
: >"$hold/go"
wait "$a65_copy" "$a65_skip" || true
check "and the status keeps both, the copy's at and the skip (copy $(cat "$scratch/a65-copy.status"), skip $(cat "$scratch/a65-skip.status"))" \
  bash -c '[ "$(cat "$1")" -eq 0 ] && [ "$(cat "$2")" -eq 0 ] &&
    jq -e ".at == $4 and .last_skip.at == $5 and .last_skip.what == \"copy\"" "$3/status.json" >/dev/null &&
    [ -z "$(find "$3" -maxdepth 1 -name "status.json.*")" ]' \
  _ "$scratch/a65-copy.status" "$scratch/a65-skip.status" "$root3" "$(at 4100)" "$(at 4150)"

echo "== a grown file's hint holds against the index's sha256 too (rev-e's N1)"
# The newest index says big.bin's size right and its sha256 wrong: the next
# copy reads big.bin whole rather than keep that index's blocks.
newest=$(find "$root3/frank" -mindepth 1 -maxdepth 1 -type d -name '2*Z' -printf '%f\n' | sort | tail -n 1)
nidx=$root3/frank/$newest.blocks
cp -p -- "$nidx" "$scratch/nidx.kept"
chmod 600 "$nidx"
{ head -n -1 "$scratch/nidx.kept" | jq -c 'if .path == "projects/p/big.bin" then .sha256 = ("0" * 64) else . end'
  tail -n 1 "$scratch/nidx.kept"; } >"$nidx"
head -c 1000 /dev/urandom >>"$frank/.claude/projects/p/big.bin"
s3 SPAWN_SESSION_BACKUP_NOW="$(at 4200)" -- copy
check "a grown file whose index line has another sha256 is read whole (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "frank: blocks .*, $2 bytes read" "$3"' \
  _ "$status" "$(stat -c %s "$frank/.claude/projects/p/big.bin")" "$out"
cp -- "$scratch/nidx.kept" "$nidx"
chmod 400 "$nidx"
s3 -- _check frank
check "and check passes on the store (status $status)" [ "$status" -eq 0 ]

echo "== a rewrite of the same size and mtime, and appends (rev-g's F2)"
# A: f is X. B: f is Y, the same size, given A's mtime, so B's manifest
# inherits X's sha. C: f is X again, appended to. B holds the only Y: it is
# kept, though C begins with what B's manifest says.
erin=$scratch/home/erin
root2=$scratch/backups2/spawn-sessions
mkdir -p "$erin/.claude/projects/p"
f=$erin/.claude/projects/p/f.jsonl
printf 'XXXX\n' >"$f"
touch -d '2026-10-01 00:00:00.123456789' "$f"
sb SPAWN_SESSION_BACKUP_ROOT="$root2" SPAWN_SESSION_HOMES="$erin" SPAWN_SESSION_BACKUP_NOW="$(at 0)" -- copy
printf 'YYYY\n' >"$f"
touch -d '2026-10-01 00:00:00.123456789' "$f"
sb SPAWN_SESSION_BACKUP_ROOT="$root2" SPAWN_SESSION_HOMES="$erin" SPAWN_SESSION_BACKUP_NOW="$(at 3600)" -- copy
check "B, a same-size same-mtime rewrite, is taken from A: bytes and sha (the documented limit)" \
  bash -c '[ "$(cut -d" " -f1 "$1/erin/20261101T000000Z.manifest")" = "$(cut -d" " -f1 "$1/erin/20261101T010000Z.manifest")" ] &&
    grep -qx XXXX "$1/erin/20261101T010000Z/projects/p/f.jsonl"' _ "$root2"
printf 'XXXX\nmore\n' >"$f"
sb SPAWN_SESSION_BACKUP_ROOT="$root2" SPAWN_SESSION_HOMES="$erin" SPAWN_SESSION_BACKUP_NOW="$(at 7200)" -- copy
printf 'XXXX\nmore\nand more\n' >"$f"
sb SPAWN_SESSION_BACKUP_ROOT="$root2" SPAWN_SESSION_HOMES="$erin" SPAWN_SESSION_BACKUP_NOW="$(at 10800)" -- copy
check "an appended file is the version before's bytes and the new ones" \
  bash -c 'printf "XXXX\nmore\nand more\n" | cmp -s - "$1/erin/20261101T030000Z/projects/p/f.jsonl"' _ "$root2"
for _ in 1 2; do
  sb SPAWN_SESSION_BACKUP_ROOT="$root2" SPAWN_SESSION_HOMES="$erin" SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin
done
check "the versions every file of which is in the next are gone (status $status)" \
  bash -c '[ ! -e "$1/erin/20261101T020000Z" ] && [ -d "$1/erin/20261101T030000Z" ]' _ "$root2"

echo "== every manifest is its version's files"
# consistent <root>: each kept version's files hash as its manifest says,
# and it has no other file.
consistent() {
  local cs_m cs_v cs_sha cs_size cs_mtime cs_path
  for cs_m in "$1"/*/2*Z.manifest; do
    cs_v=${cs_m%.manifest}
    while read -r cs_sha cs_size cs_mtime cs_path; do
      [ "$(sha256sum <"$cs_v/$cs_path" | cut -d' ' -f1)" = "$cs_sha" ] || { echo "$cs_v $cs_path"; return; }
    done <"$cs_m"
    [ "$(find "$cs_v" -type f | wc -l)" -eq "$(wc -l <"$cs_m")" ] || echo "$cs_v: files beyond its manifest"
  done
}
check "in both roots" bash -c 'printf "%s" "$1$2"; [ -z "$1$2" ]' _ "$(consistent "$root")" "$(consistent "$root2")"

echo "== hostile archives (rev-g's criteria): refused, and nothing written outside its own work"
mal=20261107T000000Z
tree_hash() { (cd "$1" && find . -path ./mallory -prune -o -type f -print0 | sort -z | xargs -0 -r sha256sum | sha256sum); }
before=$(tree_hash "$root")
printf '6\t1.0\tprojects/a/f\0' >"$scratch/mal.list"
hostile() { # <name> <python building the tar at sys.argv[1]>
  python3 -I -c "$2" "$scratch/$1.tar"
  sb -- _plan mallory "$mal" <"$scratch/mal.list"
  sb -- _receive mallory "$mal" "$account_uid" <"$scratch/$1.tar"
  receive_status=$status
  cp -- "$out" "$scratch/$1.out"
  sb -- _abandon mallory "$mal"
}
mk='import sys, tarfile, io
t = tarfile.open(sys.argv[1], "w")
def link(name, to):
    i = tarfile.TarInfo(name); i.type = tarfile.SYMTYPE; i.linkname = to; t.addfile(i)
def file(name):
    d = b"pwned\n"; i = tarfile.TarInfo(name); i.size = len(d); t.addfile(i, io.BytesIO(d))
'
hostile dirlink "$mk"'link("projects/a", "../../alice"); file("projects/a/f"); t.close()'
check "a symlinked directory, then a file under it (status $receive_status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "does not extract\|no file or directory" "$2"' _ "$receive_status" "$scratch/dirlink.out"
hostile abslink "$mk"'link("projects/a", "/etc"); t.close()'
check "a symlink alone (status $receive_status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "holds what is no file or directory: projects/a" "$2"' _ "$receive_status" "$scratch/abslink.out"
hostile unasked "$mk"'file("projects/b"); t.close()'
check "a file not asked for (status $receive_status)" \
  bash -c '[ "$1" -ne 0 ] && grep -q "a file not asked for: projects/b" "$2"' _ "$receive_status" "$scratch/unasked.out"
hostile dotdot "$mk"'file("../../alice/x"); t.close()'
check "a path out of its directory (status $receive_status)" [ "$receive_status" -ne 0 ]
check "every other version is as it was, and mallory has none" \
  bash -c '[ "$1" = "$2" ] && [ ! -e "$3/mallory" ] && [ ! -e "$3/../alice" ]' _ "$before" "$(tree_hash "$root")" "$root"

hostile setuid "$mk"'d = b"pwned\\n"; i = tarfile.TarInfo("projects/a/f"); i.size = len(d); i.mode = 0o6755; t.addfile(i, io.BytesIO(d)); t.close()'
check "a setuid, setgid file: its bits never reach the version (status $receive_status)" \
  bash -c '[ -z "$(find "$1/mallory" -perm /7000)" ]' _ "$root"

echo "$passed ok, $failed failed"
[ "$failed" -eq 0 ]
