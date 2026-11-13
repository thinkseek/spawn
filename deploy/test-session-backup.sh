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
  mkdir -p "$root" "$scratch/backups2/spawn-sessions"
  chown "$bkp_uid" "$root" "$scratch/backups2/spawn-sessions"
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
all=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
count_all=$(wc -l <<<"$all")
check "$count_all versions of alice" [ "$count_all" -eq 106 ]
# Every (path, content) ever copied: the manifests before any thinning.
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

echo "== thinning: the first run on a root only prints"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin
check "says it is print-only (status $status)" grep -q "is print-only (rule (c))" "$out"
check "would remove versions" grep -q "would remove 20261101T" "$out"
check "removed none" [ "$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d | wc -l)" -eq "$count_all" ]
check "left the marker" [ -e "$root/.thin-dry-run-done" ]
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin --dry-run
check "--dry-run after it removes none either" \
  bash -c '[ "$(find "$1" -mindepth 1 -maxdepth 1 -type d | wc -l)" -eq "$2" ] && ! grep -q "removed " "$3"' _ "$root/alice" "$count_all" "$out"
check "and says it removes nothing, not that a next one removes" \
  bash -c 'grep -q "thin --dry-run: prints what it would remove, removes nothing" "$1" && ! grep -q "the next one" "$1"' _ "$out"

echo "== thinning"
sb SPAWN_SESSION_BACKUP_NOW="$(at "$now")" -- thin
kept=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d -printf '%f\n' | sort)
check "removed versions (status $status; $(wc -l <<<"$kept") kept of $count_all)" \
  bash -c '[ "$1" -eq 0 ] && [ "$(wc -l <<<"$2")" -lt "$3" ] && grep -q "removed 20261101T" "$4"' _ "$status" "$kept" "$count_all" "$out"
check "nothing ever copied is lost" bash -c '[ -z "$1" ]' _ "$(lost)"
check "the last version before b.jsonl's deletion at hour 3 is kept" grep -qx 20261101T024500Z <<<"$kept"
check "and holds b.jsonl" [ -f "$root/alice/20261101T024500Z/projects/p1/b.jsonl" ]
check "the last version before c.jsonl's rewrite at hour 5 is kept" grep -qx 20261101T044500Z <<<"$kept"
check "and those after the rewrite and in the 48 hours' days, every file of which is in the next, are gone" \
  bash -c '! grep -qx 20261103T120000Z <<<"$1"' _ "$kept"
check "a version every file of which is in the next is gone" bash -c '! grep -qx 20261101T060000Z <<<"$1"' _ "$kept"
check "its manifest and counts are gone with it" \
  bash -c '[ ! -e "$1.manifest" ] && [ ! -e "$1.counts" ]' _ "$root/alice/20261101T060000Z"
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
before=$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d | wc -l)
sb SPAWN_SESSION_BACKUP_NOW="$(at $((5 * 86400 + 900)))" -- copy
check "copied, saying SHRANK (status $status)" \
  bash -c '[ "$1" -eq 0 ] && grep -q "SHRANK alice: 2 files now, 5 in 20261105T231500Z" "$2"' _ "$status" "$out"
check "recorded in its counts and status.json" \
  bash -c 'jq -e ".shrank" "$1/alice/20261106T001500Z.counts" >/dev/null &&
    jq -e "[.accounts[] | select(.account == \"alice\")][0].shrank" "$1/status.json" >/dev/null' _ "$root"
check "every earlier version is kept" [ "$(find "$root/alice" -mindepth 1 -maxdepth 1 -type d | wc -l)" -eq $((before + 1)) ]

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
