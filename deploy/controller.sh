#!/usr/bin/env bash
# The controller: the deployed commit's tree, from which the live system runs
# the gate, its scenarios, the objective backlog and the maintenance script.
# Never the owner's checkout: an owner edit is live only after its deploy.
#
#   deploy/controller.sh build  <name> <git-dir> <commit> [<web-node-modules>]
#   deploy/controller.sh switch <name> <commit>
#   deploy/controller.sh update <name> <git-dir> <commit> [<web-node-modules>]
#   deploy/controller.sh path   <name>
#
# Layout, in $SPAWN_LIVE/controllers (default ~/.local/share/spawn-rsi):
#
#   <name>-<commit>/    one directory per commit: `git archive <commit>`,
#                       read-only (every file and directory but the
#                       node_modules below), with .controller naming the
#                       commit, written last;
#   <name>              symlink to the current one: what everything live names
#                       (the integrator's SPAWN_GATE, the missions' --gate and
#                       --backlog, spawn-maintenance's ExecStart);
#   <name>.prev         symlink to the one before it.
#
# build makes <name>-<commit> unless it is already there (in a temporary
# directory, then one rename: a half-built tree never has the final name).
# switch makes it current: the new symlink is made beside the old one and
# renamed over it (rename(2) is atomic; `ln -sfn` is unlink + symlink, and a
# reader in between would find nothing). update is both.
#
# What holds the previous controller keeps it. A reader resolves the symlink
# once and then reads the directory, not the link: the integrator pins its
# gate and scenarios with canonicalize for each step, a running sh keeps its
# script open, and the gate pins the controller it found. After a switch, a
# controller that is neither <name> nor <name>.prev is removed, unless a
# process still has its working directory, its root or an open file in it
# (/proc/*/cwd, root, fd): that one goes at a later switch.
#
# <web-node-modules>, for Spawn: the gate's check-web.sh builds the UI with the
# controller's web/node_modules, which git does not hold. It is copied into
# web/node_modules as a reflink (`cp --reflink=always`: on btrfs, as here, the
# copy shares every data block and costs only metadata, a few MB for ~7500
# files, while each file is its own inode, so an edit or reinstall in the
# source never reaches the controller); on a filesystem without reflinks, as
# hardlinks (the same small cost, but the inodes are shared: pnpm replaces
# files rather than writing into them, an editor may not); across
# filesystems, a full copy, with a warning. A source whose installed lockfile
# (.pnpm/lock.yaml) is not the commit's web/pnpm-lock.yaml is refused: the
# gate would judge with other dependencies than the commit names.
#
# One build or switch per name at a time (flock on controllers/.<name>.lock).
set -euo pipefail

# Messages go to stderr: build's stdout is the directory it made.
say() { printf 'controller: %s\n' "$*" >&2; }
die() {
  printf 'controller: %s\n' "$*" >&2
  exit 1
}

live=${SPAWN_LIVE:-$HOME/.local/share/spawn-rsi}
dir=$live/controllers

usage() { die "usage: deploy/controller.sh build|update <name> <git-dir> <commit> [<web-node-modules>] | switch <name> <commit> | path <name>"; }

valid_name() {
  [[ $1 =~ ^[a-z][a-z0-9-]*$ ]] || die "a controller name is lowercase letters, digits and dashes, not $1"
}

lock() { # lock <name>: held until this process exits
  mkdir -p "$dir"
  exec 8>>"$dir/.$1.lock"
  flock -w 600 8 || die "another build or switch of $1 holds $dir/.$1.lock"
}

# writable <tree> u+w|a-w: every directory and file of the git tree, never
# the node_modules (its inodes may be the source's: hardlinks).
set_mode() {
  find "$1" -path "$1/web/node_modules" -prune -o \( -type d -o -type f \) -exec chmod "$2" {} +
}

remove_tree() {
  [ -e "$1" ] || return 0
  set_mode "$1" u+w 2>/dev/null || true
  rm -rf "$1"
}

# in_use <directory>: a process has its working directory, root or an open
# file there. Processes of other users are unreadable and skipped (they
# cannot be the integrator's).
in_use() {
  local hit
  hit=$(find /proc/[0-9]*/cwd /proc/[0-9]*/root /proc/[0-9]*/fd -maxdepth 1 \
    \( -lname "$1" -o -lname "$1/*" \) -print -quit 2>/dev/null) || true
  [ -n "$hit" ]
}

copy_modules() { # copy_modules <source> <destination>
  if cp -a --reflink=always "$1" "$2" 2>/dev/null; then
    say "web/node_modules: reflinked from $1"
    return
  fi
  rm -rf "$2"
  if cp -al "$1" "$2" 2>/dev/null; then
    say "web/node_modules: hardlinked from $1 (no reflinks here)"
    return
  fi
  rm -rf "$2"
  cp -a "$1" "$2"
  printf 'controller: warning: web/node_modules is a full copy of %s (%s): not on the same filesystem\n' \
    "$1" "$(du -sh "$2" | cut -f1)" >&2
}

# build <name> <git-dir> <commit> [<web-node-modules>]: prints the directory.
build() {
  local name=$1 git_dir=$2 commit=$3 modules=${4-}
  valid_name "$name"
  commit=$(git --git-dir="$git_dir" rev-parse --verify --quiet "$commit^{commit}") ||
    die "no commit $3 in $git_dir"
  local final=$dir/$name-$commit
  lock "$name"
  if [ -f "$final/.controller" ] && [ "$(cat "$final/.controller")" = "$commit" ]; then
    say "$name-$commit is already built"
    printf '%s\n' "$final"
    return
  fi
  [ ! -e "$final" ] || die "$final exists but is not a complete controller (no .controller naming $commit): look at it and remove it"
  if [ -n "$modules" ]; then
    modules=$(readlink -f "$modules") && [ -d "$modules" ] || die "no node_modules directory $4"
    if [ -f "$modules/.pnpm/lock.yaml" ] &&
      ! git --git-dir="$git_dir" show "$commit:web/pnpm-lock.yaml" 2>/dev/null | cmp -s - "$modules/.pnpm/lock.yaml"; then
      die "$modules was installed from another lockfile than $commit's web/pnpm-lock.yaml: run pnpm install --frozen-lockfile there first"
    fi
  fi
  local tmp
  tmp=$(mktemp -d "$dir/.$name-$commit.tmp.XXXXXX")
  # shellcheck disable=SC2064
  trap "remove_tree '$tmp'" EXIT
  git --git-dir="$git_dir" archive "$commit" | tar -x -C "$tmp"
  if [ -n "$modules" ]; then
    [ -d "$tmp/web" ] || die "$commit has no web/ for $modules"
    [ ! -e "$tmp/web/node_modules" ] || die "$commit tracks web/node_modules"
    copy_modules "$modules" "$tmp/web/node_modules"
  fi
  printf '%s\n' "$commit" >"$tmp/.controller"
  set_mode "$tmp" a-w
  chmod u+w "$tmp" # the rename below needs only the parent; this keeps rm simple
  mv -T "$tmp" "$final"
  chmod a-w "$final"
  trap - EXIT
  say "built $name-$commit"
  printf '%s\n' "$final"
}

# switch <name> <commit>: <name> -> <name>-<commit>, atomically; the one it
# replaces becomes <name>.prev; then the cleanup.
switch() {
  local name=$1 commit=$2
  valid_name "$name"
  [[ $commit =~ ^[0-9a-f]{40}$ ]] || die "switch takes a full commit id, not $commit"
  local target=$name-$commit
  lock "$name"
  [ -f "$dir/$target/.controller" ] && [ "$(cat "$dir/$target/.controller")" = "$commit" ] ||
    die "$dir/$target is not built: deploy/controller.sh build first"
  local current=""
  [ -L "$dir/$name" ] && current=$(readlink "$dir/$name")
  [ -L "$dir/$name" ] || [ ! -e "$dir/$name" ] || die "$dir/$name exists and is not a symlink"
  if [ "$current" = "$target" ]; then
    say "$name is already $target"
  else
    if [ -n "$current" ]; then
      ln -sfn "$current" "$dir/.$name.prev.new"
      mv -T "$dir/.$name.prev.new" "$dir/$name.prev"
    fi
    ln -sfn "$target" "$dir/.$name.new"
    mv -T "$dir/.$name.new" "$dir/$name"
    say "$name -> $target${current:+ (was $current)}"
  fi
  cleanup "$name"
}

cleanup() {
  local name=$1 keep_current keep_prev="" entry base
  keep_current=$(readlink "$dir/$name")
  [ -L "$dir/$name.prev" ] && keep_prev=$(readlink "$dir/$name.prev")
  for entry in "$dir/$name"-* "$dir/.$name"-*.tmp.*; do
    [ -e "$entry" ] || continue
    base=${entry##*/}
    case $base in
    "$name-"[0-9a-f]*) [[ ${base#"$name-"} =~ ^[0-9a-f]{40}$ ]] || continue ;;
    ".$name-"*.tmp.*) ;;
    *) continue ;;
    esac
    [ "$base" = "$keep_current" ] || [ "$base" = "$keep_prev" ] && continue
    if in_use "$entry"; then
      say "kept $base: a process still uses it"
      continue
    fi
    remove_tree "$entry"
    say "removed $base"
  done
}

[ $# -ge 1 ] || usage
verb=$1
shift
case $verb in
build)
  [ $# -eq 3 ] || [ $# -eq 4 ] || usage
  build "$@"
  ;;
switch)
  [ $# -eq 2 ] || usage
  switch "$@"
  ;;
update)
  [ $# -eq 3 ] || [ $# -eq 4 ] || usage
  built=$(build "$@")
  switch "$1" "${built##*-}"
  ;;
path)
  [ $# -eq 1 ] || usage
  valid_name "$1"
  printf '%s\n' "$dir/$1"
  ;;
*) usage ;;
esac
