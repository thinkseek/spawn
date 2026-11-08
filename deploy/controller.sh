#!/usr/bin/env bash
# The controller: the deployed commit's tree, from which the live system runs
# the gate, its scenarios, the objective backlog and the maintenance script.
# Never the owner's checkout: an owner edit is live only after its deploy.
#
#   deploy/controller.sh build  <name> <git-dir> <commit> [<web-node-modules>]
#   deploy/controller.sh switch <name> <commit>
#   deploy/controller.sh update <name> <git-dir> <commit> [<web-node-modules>]
#   deploy/controller.sh deps   <name> <git-dir> <commit>
#   deploy/controller.sh deps-prune <name>
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
# A commit with a pnpm-lock.yaml at its root also has a deps store: the
# dependencies its tree runs on, which git does not hold. `deps` installs one
# tree per lockfile hash at <live>/deps/<name>/<sha256 of the lockfile>, a
# checkout of the commit with `pnpm install --frozen-lockfile`, read-only,
# with .deps naming the hash. <live>/deps/<name>/current points at the tree
# of the current controller's commit; `build` refuses a commit whose tree is
# missing, and `update` installs it first. The store is kept across
# lockfile changes: a tree that neither controller (current, previous) nor a
# workspace (a ws/<agent>/node_modules/.pnpm symlink into it) nor a process
# uses is removed at a switch; an agent's workspace therefore keeps the
# dependencies it built with until the workspace is gone. The owner's
# checkout is never the source: pnpm would put the same files in the owner's
# and the tree's node_modules, and every agent could write them.
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
deps=$live/deps

usage() { die "usage: deploy/controller.sh build|update <name> <git-dir> <commit> [<web-node-modules>] | switch <name> <commit> | deps <name> <git-dir> <commit> | deps-prune <name> | path <name>"; }

valid_name() {
  [[ $1 =~ ^[a-z][a-z0-9-]*$ ]] || die "a controller name is lowercase letters, digits and dashes, not $1"
}

locked=''
lock() { # lock <name>: held until this process exits (once per shell)
  [ "$locked" = "$1" ] && return 0
  mkdir -p "$dir"
  exec 8>>"$dir/.$1.lock"
  flock -w 600 8 || die "another build or switch of $1 holds $dir/.$1.lock"
  locked=$1
}

# writable <tree> u+w|a-w: every directory and file of the git tree, never
# the node_modules (its inodes may be the source's: hardlinks).
set_mode() {
  find "$1" -path "$1/web/node_modules" -prune -o \( -type d -o -type f \) -exec chmod "$2" {} +
}

# set_mode_all <tree> u+w|a-w: every file and directory, the node_modules
# included: a deps tree holds no hardlink into anyone else's (checked when it
# is installed), so this reaches no other inode.
set_mode_all() {
  find "$1" \( -type d -o -type f \) -exec chmod "$2" {} +
}

remove_tree() {
  [ -e "$1" ] || return 0
  set_mode "$1" u+w 2>/dev/null || true
  set_mode_all "$1" u+w 2>/dev/null || true
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

# deps_sha <git-dir> <commit>: the sha256 of the commit's root pnpm-lock.yaml,
# which names its tree; nothing when the commit has none.
deps_sha() {
  git --git-dir="$1" cat-file -e "$2:pnpm-lock.yaml" 2>/dev/null || return 0
  git --git-dir="$1" show "$2:pnpm-lock.yaml" | sha256sum | cut -d' ' -f1
}

# deps <name> <git-dir> <commit>: installs the commit's deps tree unless it is
# there, and prints its directory. Network, as `pnpm install` needs it: the
# owner's procedure, never a gate's.
deps() {
  local name=$1 git_dir=$2 commit=$3 sha root final pnpm tmp
  valid_name "$name"
  commit=$(git --git-dir="$git_dir" rev-parse --verify --quiet "$commit^{commit}") ||
    die "no commit $3 in $git_dir"
  sha=$(deps_sha "$git_dir" "$commit")
  [ -n "$sha" ] || die "$commit has no pnpm-lock.yaml at its root: no deps to install"
  root=$deps/$name
  final=$root/$sha
  lock "$name"
  if [ -f "$final/.deps" ] && [ "$(cat "$final/.deps")" = "$sha" ]; then
    say "deps $name-$sha is already installed"
    printf '%s\n' "$final"
    return
  fi
  [ ! -e "$final" ] || die "$final exists but is not a complete deps tree (no .deps naming $sha): look at it and remove it"
  pnpm=${SPAWN_PNPM:-pnpm}
  command -v "$pnpm" >/dev/null 2>&1 || die "$pnpm is not on PATH: it installs a deps tree"
  mkdir -p "$root"
  tmp=$(mktemp -d "$root/.$sha.tmp.XXXXXX")
  # shellcheck disable=SC2064
  trap "remove_tree '$tmp'" EXIT
  git --git-dir="$git_dir" archive "$commit" | tar -x -C "$tmp"
  # clone-or-copy: pnpm's hardlink import method shares the global store's
  # inodes, and a read-only tree would make those files read-only for every
  # tree and the store too. A clone (reflink) or a copy is the tree's own; the
  # check below refuses anything else.
  #
  # --ignore-scripts: --frozen-lockfile freezes the lockfile, not the
  # dependencies' lifecycle scripts, and this runs as the owner with network:
  # a postinstall or prepare in the commit's root or in a workspace package
  # (the protected list is a prefix list and could grow to cover one) would
  # run here. Nothing in the tree needs one: pnpm-workspace.yaml sets
  # `allowBuilds: esbuild: false`, and esbuild's binary comes from its
  # platform package, not its postinstall. A new dependency that does need a
  # build step is an owner decision, and a reason to revisit this line.
  ( cd "$tmp" && "$pnpm" install --frozen-lockfile --ignore-scripts --config.package-import-method=clone-or-copy ) >&2 ||
    die "pnpm install failed in $tmp"
  # The install is the commit's: the export is the commit's tree, so the
  # lockfile pnpm froze against is the commit's (`--frozen-lockfile` refuses
  # one that disagrees with the manifests). pnpm's own copy of the lockfile
  # in node_modules/.pnpm is pruned to this platform, so it is not compared
  # byte for byte.
  [ -f "$tmp/node_modules/.pnpm/lock.yaml" ] ||
    die "$tmp has no node_modules/.pnpm/lock.yaml after pnpm install"
  # No file may share an inode with the store (see clone-or-copy above): the
  # chmod below would reach it.
  if find "$tmp" -type f -links +1 -print -quit | grep -q .; then
    die "$tmp has files hardlinked outside it (pnpm imported from the store as hardlinks): the tree could not be made read-only without changing the store"
  fi
  printf '%s\n' "$sha" >"$tmp/.deps"
  set_mode_all "$tmp" a-w
  chmod u+w "$tmp"
  mv -T "$tmp" "$final"
  chmod a-w "$final"
  trap - EXIT
  say "installed deps $name-$sha"
  printf '%s\n' "$final"
}

# deps_point <name> <sha>: <live>/deps/<name>/current -> <sha>, atomically.
deps_point() {
  local name=$1 sha=$2
  [ -n "$sha" ] && [ -d "$deps/$name/$sha" ] || return 0
  ln -sfn "$sha" "$deps/$name/.current.new"
  mv -T "$deps/$name/.current.new" "$deps/$name/current"
}

# deps_linked <tree>: a workspace's node_modules/.pnpm symlink resolves into
# it (what `gate/scenarios/build` makes in an agent's workspace).
deps_linked() {
  local ws target
  for ws in "$live"/ws/*/node_modules/.pnpm; do
    [ -L "$ws" ] || continue
    target=$(readlink -f "$ws" 2>/dev/null) || continue
    case "$target" in
    "$1"/*) return 0 ;;
    esac
  done
  return 1
}

# deps_prune <name>: keeps the trees the current and previous controllers name
# (their .deps), a tree a workspace links to and one a process uses; removes
# the rest.
deps_prune() {
  local name=$1 root keep_current='' keep_prev='' entry base
  root=$deps/$name
  [ -d "$root" ] || return 0
  if [ -f "$dir/$name/.deps" ]; then keep_current=$(cat "$dir/$name/.deps"); fi
  if [ -L "$dir/$name.prev" ] && [ -f "$dir/$name.prev/.deps" ]; then
    keep_prev=$(cat "$dir/$name.prev/.deps")
  fi
  for entry in "$root"/*; do
    [ -d "$entry" ] || continue
    base=${entry##*/}
    [[ $base =~ ^[0-9a-f]{64}$ ]] || continue
    if [ "$base" = "$keep_current" ] || [ "$base" = "$keep_prev" ]; then
      continue
    fi
    if deps_linked "$entry"; then
      say "kept deps/$name/$base: an agent's workspace still links to it"
      continue
    fi
    if in_use "$entry"; then
      say "kept deps/$name/$base: a process still uses it"
      continue
    fi
    remove_tree "$entry"
    say "removed deps/$name/$base"
  done
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
  local sha
  sha=$(deps_sha "$git_dir" "$commit")
  if [ -n "$sha" ] && { [ ! -f "$deps/$name/$sha/.deps" ] || [ "$(cat "$deps/$name/$sha/.deps")" != "$sha" ]; }; then
    die "$commit has a pnpm-lock.yaml at its root, and no deps tree $deps/$name/$sha: deploy/controller.sh deps $name $git_dir $commit"
  fi
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
  if [ -n "$sha" ]; then printf '%s\n' "$sha" >"$tmp/.deps"; fi
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
  local sha=''
  if [ -f "$dir/$target/.deps" ]; then sha=$(cat "$dir/$target/.deps"); fi
  deps_point "$name" "$sha"
  deps_prune "$name"
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
  # A commit with a root pnpm-lock.yaml needs its deps tree first: install it
  # when this is the first update to that lockfile (the owner's procedure is
  # allowed the network), and refuse rather than switch to a controller the
  # gate cannot build.
  sha=$(deps_sha "$2" "$3")
  if [ -n "$sha" ] && [ ! -f "$deps/$1/$sha/.deps" ]; then
    deps "$1" "$2" "$3" >/dev/null
  fi
  built=$(build "$@")
  switch "$1" "${built##*-}"
  ;;
deps)
  [ $# -eq 3 ] || usage
  deps "$@"
  ;;
deps-prune)
  [ $# -eq 1 ] || usage
  valid_name "$1"
  lock "$1"
  deps_prune "$1"
  ;;
path)
  [ $# -eq 1 ] || usage
  valid_name "$1"
  printf '%s\n' "$dir/$1"
  ;;
*) usage ;;
esac
