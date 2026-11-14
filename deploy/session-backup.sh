#!/usr/bin/env bash
# Session backups (core (i), sync/proposal-session-backups-rev-f.md): every
# account's native Claude Code sessions, copied with versions to where a
# mistake made with that account's HOME cannot reach.
#
#   deploy/session-backup.sh copy               # one version per account, now
#   deploy/session-backup.sh thin [--dry-run]   # drop versions no file needs
#   deploy/session-backup.sh restore [--from-blocks] <account> <version> <empty scratch dir>
#   deploy/session-backup.sh drill              # restore the newest into scratch, compare
#   deploy/session-backup.sh list [<account>]   # the versions, with their counts
#
# Why: on 2026-11-12 at 00:37:11 a test run removed /home/ckl/.claude/projects
# (sync/incident-2026-11-12-claude-projects/); no copy existed.
#
# Settings, from the unit (deploy/systemd/spawn-session-backup.*), never
# defaulted:
#   SPAWN_SESSION_BACKUP_ROOT  where the versions go: an absolute path whose
#                              last component is `spawn-sessions`, that is
#                              not, does not hold and does not lie in any
#                              home it copies;
#   SPAWN_SESSION_HOMES        the homes to copy, separated by `:`. Each
#                              one's account is its last component.
#   SPAWN_SESSION_BACKUP_USER  run as root (the system unit): the user that
#                              owns the root and every version in it
#                              (`spawn-backup`: a system user, no login, no
#                              home, no supplementary group; the root is made
#                              for it at install). Run as an account (the
#                              user units, the test), that account owns them,
#                              and this, if set, must name it.
# Optional: TMPDIR (the drill's scratch).
#
# Who does what (rev-g's review, F1 and F3). Three parties, and none writes
# where another can:
#   - the account reads its own sessions, as itself: it lists them and
#     archives the files the backup user asks for (tar -c);
#   - the backup user writes its own tree only: it extracts that archive
#     (tar -x --no-same-owner) into a directory of its own that the account
#     cannot reach, refuses an archive that holds anything but files and
#     directories or a file it did not ask for, builds the version, and does
#     every manifest, thinning and status;
#   - root only starts the other two (setpriv: the user's uid and group, no
#     supplementary group, no new privileges) and joins their pipes. It
#     reads no file's content (it reads the owner of each home's .claude,
#     to know whom to start) and writes no file.
# Restore is the reverse: the backup user archives a version, the account
# extracts it into a directory it made (empty), and hashes what it wrote;
# those hashes are compared with the version's manifest. Run as an account,
# that account is all three, and only its own homes are copied.
#
# What a version is. <root>/<account>/<UTC stamp>/ holds the regular files
# of the account's .claude/projects, .claude/history.jsonl and
# .claude/file-history (those that exist; a symlink in them is not copied).
# It is made as <stamp>.partial and renamed when complete, so a half-made
# copy never has a version's name. Only the files that changed are sent: one
# whose size and mtime are those of the version before is taken from that
# version (a reflink copy, which on this host's btrfs shares its data); one
# that grew and begins with that version's bytes is a reflink of it with the
# new bytes appended (a transcript is only appended to); any other is the
# archive's. A rewrite that keeps a file's size and its mtime to the ns is
# not seen. Beside each version:
#   <stamp>.manifest  one line per file: sha256, size, mtime (the account's,
#                     to the ns), path (in the version);
#   <stamp>.counts    the account's uid, files, dirs and bytes in the
#                     version, the bytes it adds, whether it shrank, the
#                     files gone between listing and archive, the version
#                     before it.
# A copy whose files are fewer than 90 % of the version before it (a
# shrink, as at 00:37, seen by count and not only by bytes) says so
# (`SHRANK`) and in its counts (`shrank`), and is kept like any other:
# every version before it stays.
#
# A home whose .claude, or one of the three items in it, is a symlink or is
# not the account's own (the account is .claude's owner, never root) is
# refused, named on stderr (`REFUSED`), and the run fails; the other homes
# are copied.
#
# After a copy or a thinning, <root>/status.json (0644; the root is 0711,
# every account's directory 0700) tells the owner's alert step each
# account's newest version and counts, the bytes the kept versions are
# estimated to use (what each added to the one before it), and the free
# bytes where the root is. Every write of it (a copy's, a thinning's, a
# skip's) holds the status lock, an flock on <root>/status.lock (0600) held
# only while status.json is read and published under a temporary name of
# its own, so no update is lost or mixed (cc-astra's A65).
#
# Thinning keeps every version of the last 48 hours, and of each UTC day
# before that its last version. Any other version goes only when the
# version after it holds every one of its files: the same path, with the
# same content or with that content as its beginning. A file that was
# deleted, or rewritten other than by an append, before the next version
# keeps its version for good. So nothing that was ever copied is lost by
# thinning. Each removal is that check passing at the moment of the
# removal. The first thinning of a root is print-only (rule (c),
# sync/cc-rules.md): it prints what it would remove, leaves
# `.thin-dry-run-done` and removes nothing; later runs remove. With
# --dry-run it only prints, always.
#
# Restore copies one version into a directory that is empty or absent,
# never inside a home it copies (restoring onto the live sessions is the
# owner's step, by hand, from that scratch copy), and checks every file
# against the manifest. The drill restores each account's newest version
# into a scratch directory the account makes, checks it, has the account
# remove it, and prints one line for sync/recovery.md.
#
# Blocks (objective 147, stage 1 of sync/design-143-session-block-store.md).
# Every new version is also written as blocks beside its directory, by
# deploy/session-blocks.py (beside this script, as the backup user):
# <account>/blocks/<h1h2>/<sha256>.zst, each distinct 64 KiB of a file once,
# zstd -3, 0400, and <stamp>.blocks, the version's index of them. The copy
# tells it how each file was made (taken whole from the version before, or
# grown from it), so an unchanged file reuses the version before's block
# list and a grown one writes only its new tail. The directory stays the
# version: restore, thinning and every check above read it as before. A
# block write that fails says BLOCKS FAILED and leaves the version without
# blocks; the version itself is kept. Thinning removes a version's .blocks
# with it; a block no index names is left (stage 5's sweep, not this one).
# A version's index is published (<stamp>.blocks.partial renamed) only once
# the version is committed, so an index never names a version that is not
# one (a kill before it leaves a .partial that check reports).
# Redaction: the redaction tools change a version's directory and manifest,
# not its blocks, so the values they remove would stay in the blocks. No
# redaction may run on a version written with blocks until a block
# redaction exists (design 143 section 5); check reports such a version
# (its index then disagrees with its manifest).
# `restore --from-blocks` rebuilds a version from its blocks instead (each
# block checked against its name, each file against its index), and the
# drill does both for the newest version and compares the two copies byte
# for byte, then checks the account's blocks whole (session-blocks.py
# check: every index parses and agrees with its manifest, every block named
# hashes to its name, every distinct file rebuilds to its sha256).
#
# The backup lock (objective 147). copy and thin take an exclusive flock on
# the root directory itself; restore and the drill take it shared. It is
# taken through a descriptor of the directory guard() checked, so no lock
# file is made and root writes nothing. A copy or thin that cannot have it
# within SPAWN_SESSION_BACKUP_LOCK_WAIT seconds (600) does nothing, says
# SKIPPED, and records the skip in status.json (`last_skip`); status.json's
# `at` stays the last real copy's, so a long hold reads as a stale backup. A
# restore or drill that cannot have it fails. A process started as an
# account never holds the descriptor. The redaction tools
# (/usr/local/lib/spawn/redact) do not take the lock: the redaction runbook
# keeps them from running beside a copy, and their lock is a later change.
#
# Variables. Every function's names carry its prefix (as_, gd_, ls_, pl_,
# rc_, ab_, cp_, st_, ok_, th_, nw_, li_, rs_, dr_, mn_, lk_, sk_): sh scopes no variable
# to a function, and a helper's variable once turned a caller's `rm -rf` on
# the real transcripts (2026-11-12).
set -euo pipefail
export LC_ALL=C
self=$(readlink -f -- "${BASH_SOURCE[0]}")
blocks_tool=$(dirname -- "$self")/session-blocks.py

say() { printf 'session-backup: %s\n' "$*"; }
die() {
  printf 'session-backup: error: %s\n' "$*" >&2
  exit 2
}

# as <uid> <command...>: the command as that account. Run as root, through
# setpriv with its group, no other group and no new privileges; run as the
# account itself, as is; as anyone else, refused.
as() {
  local as_uid=$1 as_gid
  shift
  if [ "$(id -u)" = "$as_uid" ]; then
    "$@"
  elif [ "$(id -u)" = 0 ]; then
    as_gid=$(getent passwd "$as_uid" | cut -d: -f4)
    [ -n "$as_gid" ] || {
      echo "session-backup: refused: uid $as_uid has no passwd entry" >&2
      return 1
    }
    if [ -n "${lock_fd-}" ] && [ "$as_uid" != "${bkp-}" ]; then
      # An account never holds the root's descriptor (it would list the
      # root through it).
      setpriv --reuid="$as_uid" --regid="$as_gid" --clear-groups --no-new-privs -- "$@" {lock_fd}<&-
    else
      setpriv --reuid="$as_uid" --regid="$as_gid" --clear-groups --no-new-privs -- "$@"
    fi
  else
    echo "session-backup: refused: this run (uid $(id -u)) cannot act as uid $as_uid" >&2
    return 1
  fi
}

# guard: the root, the homes and the backup user as the header says, or
# nothing runs. Sets root, homes and bkp (the backup user's uid).
guard() {
  local gd_home gd_root gd_real gd_user
  [ -n "${SPAWN_SESSION_BACKUP_ROOT-}" ] || die "SPAWN_SESSION_BACKUP_ROOT is not set: nothing done"
  [ -n "${SPAWN_SESSION_HOMES-}" ] || die "SPAWN_SESSION_HOMES is not set: nothing done"
  case $SPAWN_SESSION_BACKUP_ROOT in /*/spawn-sessions) ;; *)
    die "SPAWN_SESSION_BACKUP_ROOT must be an absolute path ending in /spawn-sessions, not '$SPAWN_SESSION_BACKUP_ROOT'"
    ;;
  esac
  gd_root=$(readlink -m -- "$SPAWN_SESSION_BACKUP_ROOT")
  IFS=: read -r -a homes <<<"$SPAWN_SESSION_HOMES"
  for gd_home in "${homes[@]}"; do
    [ -n "$gd_home" ] && [ "${gd_home#/}" != "$gd_home" ] || die "a home in SPAWN_SESSION_HOMES is empty or relative"
    gd_real=$(readlink -m -- "$gd_home")
    case $gd_root/ in "$gd_real"/*) die "the backup root $gd_root lies in the home $gd_real" ;; esac
    case $gd_real/ in "$gd_root"/*) die "the backup root $gd_root holds the home $gd_real" ;; esac
  done
  root=$gd_root
  gd_user=${SPAWN_SESSION_BACKUP_USER-}
  if [ "$(id -u)" = 0 ]; then
    [ -n "$gd_user" ] || die "run as root, SPAWN_SESSION_BACKUP_USER must name the backup user: nothing done"
    bkp=$(id -u -- "$gd_user" 2>/dev/null) || die "no user $gd_user: nothing done"
    [ "$bkp" != 0 ] || die "the backup user $gd_user is root: nothing done"
    [ "$(id -G -- "$gd_user")" = "$(id -g -- "$gd_user")" ] ||
      die "the backup user $gd_user has a supplementary group: nothing done"
    [ -d "$root" ] && [ ! -L "$root" ] && [ "$(stat -c %u -- "$root")" = "$bkp" ] ||
      die "$root must be a directory of $gd_user's (made at install): nothing done"
  else
    bkp=$(id -u)
    [ -z "$gd_user" ] || [ "$(id -u -- "$gd_user" 2>/dev/null)" = "$bkp" ] ||
      die "SPAWN_SESSION_BACKUP_USER is $gd_user, and this run is uid $bkp's: nothing done"
  fi
}

# backup <command> <args...>: this script's command as the backup user.
backup() { as "$bkp" bash "$self" "$@"; }

valid_account() { [[ $1 =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]*$ ]]; }
valid_version() { [[ $1 =~ ^[0-9]{8}T[0-9]{6}Z$ ]]; }

# lock_root <-x|-s>: the backup lock, taken on the root directory through a
# descriptor (lock_fd) held until this run exits; status 1 when it is not
# had within SPAWN_SESSION_BACKUP_LOCK_WAIT seconds (600).
lock_root() {
  local lk_wait=${SPAWN_SESSION_BACKUP_LOCK_WAIT:-600}
  [[ $lk_wait =~ ^[0-9]+$ ]] || die "SPAWN_SESSION_BACKUP_LOCK_WAIT must be a number of seconds"
  [ -d "$root" ] || backup _mkroot
  exec {lock_fd}<"$root" || die "cannot open $root for the backup lock"
  flock "$1" -w "$lk_wait" "$lock_fd"
}

versions() { # <account dir>: its versions, oldest first
  find "$1" -mindepth 1 -maxdepth 1 -type d -regextype posix-extended \
    -regex '.*/[0-9]{8}T[0-9]{6}Z' -printf '%f\n' | sort
}

stamp_epoch() { # <stamp>: its UTC seconds
  date -u -d "${1:0:4}-${1:4:2}-${1:6:2} ${1:9:2}:${1:11:2}:${1:13:2}" +%s
}

# --- the account's side ------------------------------------------------------

# _list <home> (as the account): "size<TAB>mtime<TAB>path<NUL>" for every
# regular file of the three items; refused (status 3) when .claude or an
# item is a symlink or not this account's.
_list() {
  local ls_claude=$1/.claude ls_item ls_items=()
  [ ! -L "$ls_claude" ] && [ -d "$ls_claude" ] || {
    echo "$ls_claude is a symlink or no directory" >&2
    exit 3
  }
  [ "$(stat -c %u -- "$ls_claude")" = "$(id -u)" ] || {
    echo "$ls_claude is not uid $(id -u)'s" >&2
    exit 3
  }
  for ls_item in projects history.jsonl file-history; do
    [ -e "$ls_claude/$ls_item" ] || [ -L "$ls_claude/$ls_item" ] || continue
    [ ! -L "$ls_claude/$ls_item" ] && [ "$(stat -c %u -- "$ls_claude/$ls_item")" = "$(id -u)" ] || {
      echo "$ls_claude/$ls_item is a symlink or not uid $(id -u)'s" >&2
      exit 3
    }
    ls_items+=("$ls_item")
  done
  [ "${#ls_items[@]}" -gt 0 ] || exit 0
  cd "$ls_claude"
  find "${ls_items[@]}" -type f -printf '%s\t%T@\t%p\0'
}

# _send <home> (as the account): an archive of the paths read on stdin
# (one a line, taken verbatim, relative to .claude); a path gone since the
# listing is left out.
_send() {
  cd "$1/.claude"
  tar -c --verbatim-files-from --no-recursion --ignore-failed-read -T - -f -
}

# --- the backup user's side --------------------------------------------------

# _plan <account> <stamp> (as the backup user): reads the account's listing,
# keeps its valid entries in <account>/.incoming-<stamp>/list, and prints
# the paths whose size or mtime differ from the version before, one a line
# (a valid path has no newline).
_plan() {
  local pl_account=$1 pl_stamp=$2 pl_dir pl_work pl_prev pl_size pl_mtime pl_path pl_sha
  local -A pl_known=()
  valid_account "$pl_account" && valid_version "$pl_stamp" || die "_plan: bad arguments"
  pl_dir=$root/$pl_account
  [ -d "$pl_dir" ] || mkdir -m 700 "$pl_dir"
  [ ! -e "$pl_dir/$pl_stamp" ] || die "$pl_dir/$pl_stamp exists already"
  pl_work=$pl_dir/.incoming-$pl_stamp
  mkdir -m 700 "$pl_work"
  pl_prev=$(versions "$pl_dir" | tail -n 1)
  if [ -n "$pl_prev" ] && [ -f "$pl_dir/$pl_prev.manifest" ]; then
    while read -r pl_sha pl_size pl_mtime pl_path; do
      pl_known[$pl_path]="$pl_size $pl_mtime"
    done <"$pl_dir/$pl_prev.manifest"
  fi
  : >"$pl_work/list"
  while IFS=$'\t' read -r -d '' pl_size pl_mtime pl_path; do
    [[ $pl_size =~ ^[0-9]+$ ]] && [[ $pl_mtime =~ ^[0-9]+(\.[0-9]+)?$ ]] || continue
    case $pl_path in projects/* | file-history/* | history.jsonl) ;; *) continue ;; esac
    case /$pl_path/ in */../* | */./* | *//*) continue ;; esac
    case $pl_path in *$'\n'* | *$'\t'*) continue ;; esac
    printf '%s %s %s\n' "$pl_size" "$pl_mtime" "$pl_path" >>"$pl_work/list"
    [ "${pl_known[$pl_path]-}" = "$pl_size $pl_mtime" ] || printf '%s\n' "$pl_path"
  done
}

# _receive <account> <stamp> <uid> (as the backup user): extracts the
# account's archive into .incoming-<stamp>/files, refuses it unless it holds
# only directories and files asked for, builds the version from it and the
# version before, and writes the version's manifest and counts.
_receive() {
  local rc_account=$1 rc_stamp=$2 rc_uid=$3 rc_dir rc_work rc_part rc_prev rc_pdir rc_size rc_mtime rc_path
  local rc_psize rc_pmtime rc_psha rc_line rc_sha rc_vanished=0 rc_files rc_dirs rc_bytes rc_new rc_pfiles=0
  local rc_shrank rc_bad rc_blocks rc_blocks_ok
  local -A rc_known=()
  valid_account "$rc_account" && valid_version "$rc_stamp" && [[ $rc_uid =~ ^[0-9]+$ ]] || die "_receive: bad arguments"
  rc_dir=$root/$rc_account
  rc_work=$rc_dir/.incoming-$rc_stamp
  rc_part=$rc_dir/$rc_stamp.partial
  [ -d "$rc_work" ] && [ -f "$rc_work/list" ] || die "_receive: no plan for $rc_account $rc_stamp"
  mkdir -m 700 "$rc_work/files"
  tar -x --no-same-owner -C "$rc_work/files" -f - 2>"$rc_work/tar.err" ||
    die "its archive does not extract: $(head -n 3 "$rc_work/tar.err" | tr '\n' ' ')"
  rc_bad=$(find "$rc_work/files" -mindepth 1 ! -type f ! -type d -printf '%P ' | head -c 300)
  [ -z "$rc_bad" ] || die "its archive holds what is no file or directory: $rc_bad"
  rc_bad=$(find "$rc_work/files" -mindepth 1 -perm /7000 -printf '%P ' | head -c 300)
  [ -z "$rc_bad" ] || die "its archive holds a setuid, setgid or sticky entry: $rc_bad"
  rc_bad=$(comm -23 <(find "$rc_work/files" -type f -printf '%P\n' | sort) \
    <(awk '{ p = $0; sub(/^[^ ]+ [^ ]+ /, "", p); print p }' "$rc_work/list" | sort) | head -n 3 | tr '\n' ' ')
  [ -z "$rc_bad" ] || die "its archive holds a file not asked for: $rc_bad"
  rc_prev=$(versions "$rc_dir" | tail -n 1)
  rc_pdir=${rc_prev:+$rc_dir/$rc_prev}
  if [ -n "$rc_prev" ] && [ -f "$rc_pdir.manifest" ]; then
    while read -r rc_psha rc_psize rc_pmtime rc_path; do
      rc_known[$rc_path]="$rc_psize $rc_pmtime $rc_psha"
    done <"$rc_pdir.manifest"
  fi
  mkdir -m 700 "$rc_part"
  : >"$rc_part.manifest.partial"
  : >"$rc_work/hints"
  while read -r rc_size rc_mtime rc_path; do
    rc_line=${rc_known[$rc_path]-}
    mkdir -p -- "$(dirname -- "$rc_part/$rc_path")"
    if [ -n "$rc_line" ] && [ "${rc_line% *}" = "$rc_size $rc_mtime" ]; then
      # Unchanged: the version before's, shared.
      cp --reflink=auto -p -- "$rc_pdir/$rc_path" "$rc_part/$rc_path"
      rc_sha=${rc_line##* }
      printf 'U\t%s\n' "$rc_path" >>"$rc_work/hints"
    elif [ -f "$rc_work/files/$rc_path" ]; then
      rc_size=$(stat -c %s -- "$rc_work/files/$rc_path")
      rc_psize=${rc_line%% *}
      if [ -n "$rc_line" ] && [ "$rc_psize" -le "$rc_size" ] &&
        cmp -s -n "$rc_psize" -- "$rc_pdir/$rc_path" "$rc_work/files/$rc_path"; then
        # Grown: the version before's bytes, shared, then the new ones.
        cp --reflink=auto -p -- "$rc_pdir/$rc_path" "$rc_part/$rc_path"
        tail -c +"$((rc_psize + 1))" -- "$rc_work/files/$rc_path" >>"$rc_part/$rc_path"
        printf 'G\t%s\t%s\t%s\n' "$rc_psize" "${rc_line##* }" "$rc_path" >>"$rc_work/hints"
      else
        mv -- "$rc_work/files/$rc_path" "$rc_part/$rc_path"
      fi
      rc_sha=$(sha256sum <"$rc_part/$rc_path" | cut -d' ' -f1)
    else
      # Gone between the listing and the archive.
      rc_vanished=$((rc_vanished + 1))
      continue
    fi
    printf '%s %s %s %s\n' "$rc_sha" "$rc_size" "$rc_mtime" "$rc_path" >>"$rc_part.manifest.partial"
  done <"$rc_work/list"
  sort -k4 -o "$rc_part.manifest.partial" "$rc_part.manifest.partial"
  # Its blocks, and its index as <stamp>.blocks.partial, before its
  # manifest; the index is published (renamed) only once the version is
  # committed, below (cc-astra's A63). The directory stays the version, so a
  # block write that fails is said and the copy goes on.
  rc_blocks_ok=0
  if python3 -I -B "$blocks_tool" write "$rc_dir" "$rc_stamp" "$rc_part" "$rc_part.manifest.partial" \
    "$rc_prev" "$rc_work/hints" >"$rc_work/blocks.out" 2>&1; then
    rc_blocks=$(tail -n 1 "$rc_work/blocks.out")
    rc_blocks_ok=1
  else
    rm -f -- "$rc_dir/$rc_stamp.blocks.partial" "$rc_dir/$rc_stamp.blocks"
    rc_blocks="BLOCKS FAILED $rc_account $rc_stamp: $(tail -n 2 "$rc_work/blocks.out" | tr '\n' ' ')"
  fi
  rc_files=$(find "$rc_part" -type f | wc -l)
  rc_dirs=$(find "$rc_part" -mindepth 1 -type d | wc -l)
  rc_bytes=$(awk '{ n += $2 } END { print n + 0 }' "$rc_part.manifest.partial")
  rc_new=$(new_bytes "${rc_prev:+$rc_pdir.manifest}" "$rc_part.manifest.partial")
  rc_shrank=false
  if [ -n "$rc_prev" ] && [ -f "$rc_pdir.counts" ]; then
    rc_pfiles=$(jq -r '.files' "$rc_pdir.counts")
    [ $((rc_files * 10)) -ge $((rc_pfiles * 9)) ] || rc_shrank=true
  fi
  jq -cn --argjson uid "$rc_uid" --arg v "$rc_stamp" --argjson f "$rc_files" --argjson d "$rc_dirs" \
    --argjson b "$rc_bytes" --argjson n "$rc_new" --argjson s "$rc_shrank" --arg p "$rc_prev" \
    --argjson pf "$rc_pfiles" --argjson gone "$rc_vanished" \
    '{version: $v, uid: $uid, files: $f, dirs: $d, bytes: $b, new_bytes: $n, shrank: $s, vanished: $gone,
      previous: (if $p == "" then null else {version: $p, files: $pf} end)}' >"$rc_part.counts"
  mv -- "$rc_part.counts" "$rc_dir/$rc_stamp.counts"
  mv -- "$rc_part.manifest.partial" "$rc_dir/$rc_stamp.manifest"
  # A barrier for the test only (test-session-backup.sh; no unit sets it):
  # the copy fails here, its index written and its version not committed.
  [ "${SPAWN_SESSION_BACKUP_TEST_FAIL_AT-}" != before-commit ] || die "failed before its commit (SPAWN_SESSION_BACKUP_TEST_FAIL_AT)"
  mv -T -- "$rc_part" "$rc_dir/$rc_stamp"
  # The version is committed: its index now names it.
  if [ "$rc_blocks_ok" = 1 ] &&
    ! python3 -I -B "$blocks_tool" publish "$rc_dir" "$rc_stamp" >>"$rc_work/blocks.out" 2>&1; then
    rm -f -- "$rc_dir/$rc_stamp.blocks.partial"
    rc_blocks="BLOCKS FAILED $rc_account $rc_stamp: $(tail -n 1 "$rc_work/blocks.out")"
  fi
  # The work directory _plan made for this stamp, and only it.
  case $rc_work in "$root"/*/.incoming-[0-9]*T[0-9]*Z) rm -rf -- "${rc_work:?}" ;; esac
  say "$rc_account: version $rc_stamp: $rc_files files, $rc_dirs dirs, $rc_bytes bytes, $rc_new new"
  say "$rc_account: $rc_blocks"
  if [ "$rc_shrank" = true ]; then
    say "SHRANK $rc_account: $rc_files files now, $rc_pfiles in $rc_prev (every earlier version is kept)"
  fi
}

# _abandon <account> <stamp> (as the backup user): what a failed copy left,
# its .incoming and .partial, and its account directory if that is empty.
_abandon() {
  local ab_dir
  valid_account "$1" && valid_version "$2" || die "_abandon: bad arguments"
  ab_dir=$root/$1
  [ -d "$ab_dir" ] || return 0
  rm -rf -- "${ab_dir:?}/.incoming-$2" "${ab_dir:?}/$2.partial"
  rm -f -- "${ab_dir:?}/$2.manifest.partial" "${ab_dir:?}/$2.partial.counts" "${ab_dir:?}/$2.blocks.partial"
  # Its index, unless the version was completed (its manifest written).
  [ -e "$ab_dir/$2.manifest" ] || rm -f -- "${ab_dir:?}/$2.blocks"
  rmdir --ignore-fail-on-non-empty -- "$ab_dir"
}

# new_bytes <previous manifest or ""> <manifest>: the bytes this version adds
# to the one before it, estimated: 0 for a file with the same content at the
# same path, what it grew by for a file that grew, its size otherwise. A
# reflink copy shares the rest, so the sum over the kept versions estimates
# the root's own disk use (the pilot measures it against df).
new_bytes() {
  awk 'FILENAME == ARGV[1] { p = $0; sub(/^[^ ]+ [^ ]+ [^ ]+ /, "", p); sha[p] = $1; size[p] = $2; next }
       { p = $0; sub(/^[^ ]+ [^ ]+ [^ ]+ /, "", p)
         if (!(p in sha)) n += $2
         else if (sha[p] != $1) n += ($2 > size[p] ? $2 - size[p] : $2) }
       END { print n + 0 }' "${1:-/dev/null}" "$2"
}

# status (as the backup user): <root>/status.json, readable by the owner's
# alert step: when, each account's newest version with its counts, the
# estimated bytes of every kept version (new_bytes summed), and the free
# bytes where the root is.
status() {
  local st_account_dir st_v st_counts=() st_estimate=0 st_n st_free st_blocks st_new
  for st_account_dir in "$root"/*/; do
    [ -d "$st_account_dir" ] || continue
    st_account_dir=${st_account_dir%/}
    for st_v in $(versions "$st_account_dir"); do
      st_n=$(jq -r '.new_bytes // .bytes // 0' "$st_account_dir/$st_v.counts" 2>/dev/null || echo 0)
      st_estimate=$((st_estimate + st_n))
    done
    st_v=$(versions "$st_account_dir" | tail -n 1)
    [ -n "$st_v" ] && [ -f "$st_account_dir/$st_v.counts" ] &&
      st_counts+=("$(jq -c --arg a "$(basename "$st_account_dir")" '{account: $a} + .' "$st_account_dir/$st_v.counts")")
  done
  st_free=$(df -B1 --output=avail -- "$root" | tail -n 1 | tr -d ' ')
  # The blocks' stored bytes (stage 1 adds them beside the directories).
  st_blocks=$(find "$root" -mindepth 4 -maxdepth 4 -path "$root/*/blocks/*/*.zst" -type f -printf '%s\n' |
    awk '{ n += $1 } END { print n + 0 }')
  st_new=$(printf '%s\n' "${st_counts[@]}" | jq -cs --argjson at "${SPAWN_SESSION_BACKUP_NOW:-$(date -u +%s)}" \
    --argjson e "$st_estimate" --argjson free "$st_free" --argjson blocks "$st_blocks" \
    '{at: $at, accounts: ., estimated_bytes: $e, free_bytes: $free, blocks_bytes: $blocks}') ||
    die "status: the new status could not be made"
  # The last skip is kept from the status before, read under the lock.
  status_locked status_merge "$st_new"
  chmod 711 "$root"
}

# status_merge <new status JSON> (under the status lock): the new status,
# with the last skip kept from the status before, published.
status_merge() {
  local sm_skip
  sm_skip=$(jq -c '.last_skip // null' "$root/status.json" 2>/dev/null || echo null)
  status_test_hold
  jq -cn --argjson new "$1" --argjson skip "${sm_skip:-null}" \
    '$new + (if $skip == null then {} else {last_skip: $skip} end)' | status_publish
}

# status_locked <command...>: the command under the status lock, an
# exclusive flock on <root>/status.lock held only while status.json is read
# and written. Every status write takes it (the copy's and the thinning's
# status, and a skip's, which does not have the backup lock: cc-astra's
# A65), so no update is lost or mixed.
status_locked() {
  local sl_fd sl_status=0
  [ -e "$root/status.lock" ] || (umask 077 && : >>"$root/status.lock") || die "cannot make $root/status.lock"
  exec {sl_fd}>>"$root/status.lock" || die "cannot open $root/status.lock"
  flock -x -w 60 "$sl_fd" || die "the status lock on $root is held (waited 60 s): status.json not written"
  "$@" || sl_status=$?
  exec {sl_fd}>&-
  return "$sl_status"
}

# status_publish (under the status lock): stdin, valid JSON, as
# status.json, through a temporary name of this run's own.
status_publish() {
  local sp_tmp
  sp_tmp=$(mktemp "$root/status.json.XXXXXXXX") || die "cannot make a temporary status in $root"
  if jq -e . >"$sp_tmp" && chmod 644 "$sp_tmp" && mv -- "$sp_tmp" "$root/status.json"; then
    return 0
  fi
  rm -f -- "$sp_tmp"
  die "status.json not written"
}

# status_test_hold: the test's barrier (SPAWN_SESSION_BACKUP_TEST_STATUS_HOLD,
# a directory): under the status lock, says "in" and waits up to 30 s for "go".
status_test_hold() {
  local th_dir=${SPAWN_SESSION_BACKUP_TEST_STATUS_HOLD-} th_n
  [ -n "$th_dir" ] || return 0
  : >"$th_dir/in"
  for th_n in $(seq 300); do
    [ -e "$th_dir/go" ] && return 0
    sleep 0.1
  done
}

# skipped <what> (as the backup user): the skip of a copy or thin that could
# not have the lock, in status.json's last_skip; its `at` is left as it was.
skipped() {
  local sk_at=${SPAWN_SESSION_BACKUP_NOW:-$(date -u +%s)}
  case $1 in copy | thin) ;; *) die "_skipped: copy or thin" ;; esac
  status_locked skip_merge "$sk_at" "$1"
}

# skip_merge <at> <what> (under the status lock): the status before, with
# this skip as its last_skip, published.
skip_merge() {
  if [ -f "$root/status.json" ]; then
    jq -c --argjson at "$1" --arg w "$2" '. + {last_skip: {at: $at, what: $w, because: "locked"}}' "$root/status.json"
  else
    jq -cn --argjson at "$1" --arg w "$2" '{last_skip: {at: $at, what: $w, because: "locked"}}'
  fi | status_publish
}

# holds <older version dir> <older manifest> <newer version dir>: every
# file of the older version is in the newer one at its path, with its
# content or that content as the beginning.
holds() {
  local ok_old=$1 ok_manifest=$2 ok_new=$3 ok_sha ok_size ok_mtime ok_path ok_have
  while read -r ok_sha ok_size ok_mtime ok_path; do
    [ -f "$ok_new/$ok_path" ] || return 1
    ok_have=$(stat -c %s -- "$ok_new/$ok_path")
    [ "$ok_have" -ge "$ok_size" ] || return 1
    if [ "$ok_have" -eq "$ok_size" ]; then
      cmp -s -- "$ok_old/$ok_path" "$ok_new/$ok_path" || return 1
    else
      # The bytes themselves, both files read now, never a manifest's sha
      # (rev-g's F2).
      cmp -s -n "$ok_size" -- "$ok_old/$ok_path" "$ok_new/$ok_path" || return 1
    fi
  done <"$ok_manifest"
  return 0
}

# thin [--dry-run] (as the backup user).
thin() {
  local th_dry=0 th_account_dir th_account th_now th_all th_i th_v th_next th_day th_keep th_age
  [ "${1-}" = --dry-run ] && th_dry=1
  if [ "$th_dry" = 1 ]; then
    # Asked for (the units run it so until the owner allows removal):
    # nothing is removed by this run or by any other --dry-run one.
    say "thin --dry-run: prints what it would remove, removes nothing"
  elif [ ! -e "$root/.thin-dry-run-done" ]; then
    th_dry=1
    say "the first thinning of $root is print-only (rule (c)); the next one without --dry-run removes"
  fi
  th_now=${SPAWN_SESSION_BACKUP_NOW:-$(date -u +%s)}
  for th_account_dir in "$root"/*/; do
    [ -d "$th_account_dir" ] || continue
    th_account_dir=${th_account_dir%/}
    th_account=$(basename "$th_account_dir")
    mapfile -t th_all < <(versions "$th_account_dir")
    for ((th_i = 0; th_i + 1 < ${#th_all[@]}; th_i++)); do
      th_v=${th_all[th_i]}
      th_age=$((th_now - $(stamp_epoch "$th_v")))
      [ "$th_age" -gt $((48 * 3600)) ] || continue
      th_day=${th_v:0:8}
      th_next=${th_all[th_i + 1]}
      # The day's last version is kept.
      [ "${th_next:0:8}" = "$th_day" ] || continue
      th_keep=1
      if [ -f "$th_account_dir/$th_v.manifest" ] &&
        holds "$th_account_dir/$th_v" "$th_account_dir/$th_v.manifest" "$th_account_dir/$th_next"; then
        th_keep=0
      fi
      if [ "$th_keep" = 1 ]; then
        say "$th_account: kept $th_v: a file of it is in no later version"
        continue
      fi
      if [ "$th_dry" = 1 ]; then
        say "$th_account: would remove $th_v (every file of it is in $th_next)"
        continue
      fi
      # The name is a version's (versions above), in this account's directory
      # under the guarded root.
      case $th_account_dir/$th_v in "$root"/*/[0-9]*T[0-9]*Z) ;; *) die "not a version: $th_account_dir/$th_v" ;; esac
      rm -rf -- "${th_account_dir:?}/${th_v:?}"
      rm -f -- "$th_account_dir/$th_v.manifest" "$th_account_dir/$th_v.counts" "$th_account_dir/$th_v.blocks"
      say "$th_account: removed $th_v (every file of it is in $th_next)"
    done
  done
  if [ ! -e "$root/.thin-dry-run-done" ]; then
    : >"$root/.thin-dry-run-done"
  fi
}

# As the backup user, for restore and the drill:
#   _emit <account> <version>  the version, archived;
#   _want <account> <version>  "sha256  path" of its manifest, sorted;
#   _uid <account> <version>   the uid its counts name;
#   _newest                    "account version" of each account's newest.
_emit() {
  valid_account "$1" && valid_version "$2" && [ -d "$root/$1/$2" ] || die "no version $2 of $1"
  tar -C "$root/$1/$2" -c -f - .
}
_emit_blocks() {
  valid_account "$1" && valid_version "$2" && [ -f "$root/$1/$2.blocks" ] || die "no blocks of version $2 of $1"
  python3 -I -B "$blocks_tool" emit "$root/$1" "$2"
}
_check() {
  valid_account "$1" && [ -d "$root/$1" ] || die "no account $1"
  python3 -I -B "$blocks_tool" check "$root/$1"
}
_want() {
  valid_account "$1" && valid_version "$2" && [ -f "$root/$1/$2.manifest" ] || die "no version $2 of $1"
  awk '{ p = $0; sub(/^[^ ]+ [^ ]+ [^ ]+ /, "", p); print $1 "  " p }' "$root/$1/$2.manifest" | sort
}
_uid() {
  valid_account "$1" && valid_version "$2" && [ -f "$root/$1/$2.counts" ] || die "no version $2 of $1"
  jq -er '.uid' "$root/$1/$2.counts"
}
_newest() {
  local nw_dir nw_v
  for nw_dir in "$root"/*/; do
    [ -d "$nw_dir" ] || continue
    nw_v=$(versions "${nw_dir%/}" | tail -n 1)
    [ -z "$nw_v" ] || printf '%s %s\n' "$(basename "$nw_dir")" "$nw_v"
  done
}

# list [<account>] (as the backup user).
list() {
  local li_dir li_v
  [ -z "${1-}" ] || valid_account "$1" || die "not an account: $1"
  for li_dir in "$root"/${1:-*}/; do
    [ -d "$li_dir" ] || continue
    for li_v in $(versions "${li_dir%/}"); do
      printf '%s %s %s\n' "$(basename "$li_dir")" "$li_v" "$(cat "${li_dir%/}/$li_v.counts" 2>/dev/null)"
    done
  done
}

# --- the driver (root, or the account itself): starting the two, joining ---

copy() {
  local cp_home cp_account cp_uid cp_now cp_stamp cp_failed=0 cp_out cp_need
  cp_now=${SPAWN_SESSION_BACKUP_NOW:-$(date -u +%s)}
  cp_stamp=$(date -u -d "@$cp_now" +%Y%m%dT%H%M%SZ)
  for cp_home in "${homes[@]}"; do
    cp_account=$(basename "$cp_home")
    valid_account "$cp_account" || {
      printf 'session-backup: REFUSED %s: not an account name\n' "$cp_account" >&2
      cp_failed=1
      continue
    }
    [ -d "$cp_home/.claude" ] || [ -L "$cp_home/.claude" ] || {
      say "$cp_account: no $cp_home/.claude: nothing to copy"
      continue
    }
    # Whom to start: the owner of .claude, by its own entry (a symlink's
    # own owner; the account's side refuses a symlink).
    cp_uid=$(stat -c %u -- "$cp_home/.claude")
    if [ "$cp_uid" = 0 ]; then
      printf "session-backup: REFUSED %s: %s/.claude is root's: nothing of it copied\n" "$cp_account" "$cp_home" >&2
      cp_failed=1
      continue
    fi
    # The plan first, whole (held here, never in a file), then the archive
    # of what it asks for.
    if cp_out=$({ cp_need=$(as "$cp_uid" bash "$self" _list "$cp_home" | backup _plan "$cp_account" "$cp_stamp") &&
      printf '%s' "${cp_need:+$cp_need$'\n'}" | as "$cp_uid" bash "$self" _send "$cp_home" |
      backup _receive "$cp_account" "$cp_stamp" "$cp_uid"; } 2>&1); then
      printf '%s\n' "$cp_out"
    else
      printf 'session-backup: REFUSED %s: %s: nothing of it copied\n' "$cp_account" \
        "$(printf '%s\n' "$cp_out" | sed 's/^session-backup: error: //' | head -n 3 | tr '\n' ' ')" >&2
      backup _abandon "$cp_account" "$cp_stamp" || true
      cp_failed=1
    fi
  done
  return "$cp_failed"
}

restore() {
  local rs_emit=_emit rs_from=""
  if [ "${1-}" = --from-blocks ]; then
    rs_emit=_emit_blocks rs_from=" from its blocks"
    shift
  fi
  local rs_account=${1-} rs_version=${2-} rs_target=${3-} rs_home rs_real rs_uid rs_want rs_have rs_n rs_owner
  [ -n "$rs_account" ] && [ -n "$rs_version" ] && [ -n "$rs_target" ] ||
    die "usage: restore [--from-blocks] <account> <version> <empty scratch dir>"
  valid_version "$rs_version" || die "not a version: $rs_version"
  valid_account "$rs_account" || die "not an account: $rs_account"
  rs_uid=$(backup _uid "$rs_account" "$rs_version") || die "no version $rs_version of $rs_account that names its uid"
  [ "$rs_uid" != 0 ] || die "the version $rs_version of $rs_account names uid 0: never restored as root"
  # The uid is the backup user's word: it must be the owner of that account's
  # .claude now (rev-g's R2 note), so a backup user cannot point a restore at
  # another account.
  rs_owner=""
  for rs_home in "${homes[@]}"; do
    [ "$(basename "$rs_home")" = "$rs_account" ] || continue
    rs_owner=$(stat -c %u -- "$rs_home/.claude" 2>/dev/null || true)
  done
  [ -n "$rs_owner" ] || die "$rs_account is not an account of SPAWN_SESSION_HOMES with a .claude: nothing restored"
  [ "$rs_owner" = "$rs_uid" ] || die "the version $rs_version of $rs_account names uid $rs_uid, and $rs_account's .claude is uid $rs_owner's: nothing restored"
  rs_real=$(readlink -m -- "$rs_target")
  for rs_home in "${homes[@]}"; do
    case $rs_real/ in "$(readlink -m -- "$rs_home")/.claude"/*)
      die "$rs_real is in the live sessions of $rs_home: restore into scratch, then copy by hand"
      ;;
    esac
  done
  # Every write is the account's: the target, the files, and the hashes of
  # what it wrote. The backup user only reads the version.
  as "$rs_uid" sh -c 'mkdir -p -m 700 -- "$1" && [ -d "$1" ] && [ -z "$(ls -A -- "$1")" ]' _ "$rs_real" ||
    die "$rs_real is not an empty directory uid $rs_uid can write"
  backup "$rs_emit" "$rs_account" "$rs_version" | as "$rs_uid" tar -x --no-same-owner -C "$rs_real" -f - ||
    die "the copy of $rs_account $rs_version$rs_from into $rs_real failed"
  rs_want=$(backup _want "$rs_account" "$rs_version")
  rs_have=$(as "$rs_uid" sh -c 'cd "$1" && find . -type f -printf "%P\0" | sort -z | xargs -0 -r sha256sum --' _ "$rs_real" | sort)
  if [ "$rs_want" != "$rs_have" ]; then
    die "restored $rs_account $rs_version into $rs_real, but it does not match its manifest: $(diff <(printf '%s\n' "$rs_want") <(printf '%s\n' "$rs_have") | grep '^[<>]' | head -n 3 | tr '\n' ' ')"
  fi
  rs_n=$(printf '%s\n' "$rs_want" | grep -c . || true)
  say "restored $rs_account $rs_version$rs_from into $rs_real: $rs_n files, each as its manifest says"
}

drill() {
  local dr_account dr_version dr_uid dr_scratch dr_out dr_blocks dr_cmp dr_check dr_failed=0
  while read -r dr_account dr_version; do
    if ! dr_uid=$(backup _uid "$dr_account" "$dr_version"); then
      say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: its counts name no uid"
      dr_failed=1
      continue
    fi
    # A scratch directory the account makes, and removes.
    dr_scratch=$(as "$dr_uid" mktemp -d "${TMPDIR:-/tmp}/spawn-session-drill.XXXXXX")
    if dr_out=$(restore "$dr_account" "$dr_version" "$dr_scratch/restored" 2>&1); then
      # Its blocks: the same version rebuilt from them, byte for byte the
      # directory's, and the account's blocks checked whole.
      if ! dr_blocks=$(restore --from-blocks "$dr_account" "$dr_version" "$dr_scratch/from-blocks" 2>&1); then
        say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: from its blocks: ${dr_blocks#session-backup: error: }"
        dr_failed=1
      elif ! dr_cmp=$(as "$dr_uid" diff -r -q -- "$dr_scratch/restored" "$dr_scratch/from-blocks" 2>&1); then
        say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: its blocks rebuild other bytes than its directory: $(printf '%s' "$dr_cmp" | head -n 3 | tr '\n' ' ')"
        dr_failed=1
      elif ! dr_check=$(backup _check "$dr_account" 2>&1); then
        say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: $(printf '%s' "$dr_check" | head -n 3 | tr '\n' ' ')"
        dr_failed=1
      else
        say "DRILL PASSED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: ${dr_out#session-backup: }; rebuilt from its blocks byte for byte; ${dr_check##*$'\n'}"
      fi
    else
      say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ) $dr_account $dr_version: ${dr_out#session-backup: error: }"
      dr_failed=1
    fi
    case $dr_scratch in "${TMPDIR:-/tmp}"/spawn-session-drill.??????) as "$dr_uid" rm -rf -- "${dr_scratch:?}" ;; esac
  done < <(backup _newest)
  return "$dr_failed"
}

main() {
  local mn_command=${1-} mn_status=0
  [ $# -gt 0 ] && shift
  case $mn_command in
  # The account's side, which copy starts: it needs no setting.
  _list | _send)
    "$mn_command" "$@"
    return
    ;;
  esac
  guard
  case $mn_command in
  # The backup user's side: refused unless this run is the backup user.
  _plan | _receive | _abandon | _emit | _emit_blocks | _check | _want | _uid | _newest | _status | _skipped | _mkroot)
    [ "$(id -u)" = "$bkp" ] || die "$mn_command runs as the backup user only"
    [ -d "$root" ] || { mkdir -p -- "$(dirname -- "$root")" && mkdir -m 711 -- "$root"; }
    case $mn_command in
    _status) status ;;
    _skipped) skipped "$@" ;;
    _mkroot) ;;
    *) "$mn_command" "$@" ;;
    esac
    ;;
  copy)
    if ! lock_root -x; then
      say "SKIPPED copy: the backup lock on $root is held (waited ${SPAWN_SESSION_BACKUP_LOCK_WAIT:-600} s); nothing copied"
      backup _skipped copy
      return 0
    fi
    copy || mn_status=$?
    backup _status
    return "$mn_status"
    ;;
  thin)
    if ! lock_root -x; then
      say "SKIPPED thin: the backup lock on $root is held (waited ${SPAWN_SESSION_BACKUP_LOCK_WAIT:-600} s); nothing thinned"
      backup _skipped thin
      return 0
    fi
    backup _thin "$@"
    ;;
  _thin)
    [ "$(id -u)" = "$bkp" ] || die "_thin runs as the backup user only"
    [ -d "$root" ] || { mkdir -p -- "$(dirname -- "$root")" && mkdir -m 711 -- "$root"; }
    thin "$@" && status
    ;;
  list) backup _list_versions "$@" ;;
  _list_versions)
    [ "$(id -u)" = "$bkp" ] || die "list runs as the backup user only"
    list "$@"
    ;;
  restore)
    lock_root -s || die "the backup lock on $root is held (waited ${SPAWN_SESSION_BACKUP_LOCK_WAIT:-600} s): nothing restored"
    restore "$@"
    ;;
  drill)
    if ! lock_root -s; then
      say "DRILL FAILED $(date -u +%Y-%m-%dT%H:%MZ): the backup lock on $root is held (waited ${SPAWN_SESSION_BACKUP_LOCK_WAIT:-600} s)"
      return 1
    fi
    drill
    ;;
  *) die "usage: session-backup.sh copy | thin [--dry-run] | restore [--from-blocks] <account> <version> <dir> | drill | list [<account>]" ;;
  esac
}

main "$@"
