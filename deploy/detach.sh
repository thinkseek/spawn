# shellcheck shell=bash
# A process that must outlive the unit that started it (#54, rev-e's F1).
# Sourced by deploy/deploy.sh.
#
# deploy.sh starts the base cache's warm-up (scripts/rsi/gate.sh
# --base-only) in the background and exits. Run by hand, the warm-up goes
# on. Run from a systemd unit (deploy/on-landing.sh in
# spawn-deploy-on-landing.service, a oneshot), it did not: when the unit's
# main process exits, systemd stops every process left in the unit's
# cgroup, and setsid leaves the session, not the cgroup. So the warm-up is
# a transient service of the user manager (`systemd-run --user`), which the
# caller's unit does not own: the caller's stop, its timeout included,
# never reaches it. systemd-run returns once that unit has started, so the
# caller may exit at once after it (a scope, `--scope`, is made from the
# caller's own process and is lost if the caller's unit stops first). The
# tick's unit keeps KillMode=control-group, so a hung deploy is still
# stopped whole.
#
# The warm-up's environment is exactly the assignments given, through a
# file only the owner reads (EnvironmentFile), never argv: a database URL
# among them would be in /proc/*/cmdline for anyone on the host. The file
# is removed once the unit has started (systemd read it then).
#
# detach_start <unit> <log> <assignments-file> <command...>: start
# <command> as the transient service <unit>, its output appended to <log>,
# its environment the file's KEY=value lines. 0 when it started; 1 when no
# user manager answers here (by hand, a background process outlives the
# deploy anyway: the caller starts it as before), or the unit did not start;
# 2 when <command> starts with setsid (below).
detach_start() {
  # dt_ names: no caller's (sync/cc-rules.md (b)).
  local dt_unit=$1 dt_log=$2 dt_assignments=$3
  shift 3
  # The command is the unit's main process, which systemd makes a session
  # leader. setsid there forks and its parent exits at once: systemd takes
  # the unit as done and stops it, with the command in it. So a command
  # that starts with setsid is refused (2), not started to die unseen.
  case ${1-} in setsid | */setsid)
    echo "detach_start: refused: $1 in front of the command: in a unit it forks, its parent exits and the unit stops with the command" >&2
    return 2
    ;;
  esac
  command -v systemd-run >/dev/null 2>&1 || return 1
  systemctl --user show-environment >/dev/null 2>&1 </dev/null || return 1
  systemd-run --user --quiet --collect --unit "$dt_unit" \
    -p Type=exec -p EnvironmentFile="$dt_assignments" \
    -p StandardOutput=append:"$dt_log" -p StandardError=append:"$dt_log" \
    -p WorkingDirectory="$HOME" -- "$@" </dev/null >/dev/null 2>&1
}

# detach_assignments <file> <NAME=value...>: the file detach_start reads,
# mode 600, one `NAME="value"` per line (systemd's quoting: a backslash
# and a double quote escaped; a value with a newline is left out).
detach_assignments() {
  # da_ names: no caller's (sync/cc-rules.md (b)).
  local da_file=$1 da_assignment da_value
  shift
  (umask 077 && : >"$da_file") || return 1
  for da_assignment in "$@"; do
    da_value=${da_assignment#*=}
    case $da_value in *$'\n'*) continue ;; esac
    da_value=${da_value//\\/\\\\}
    printf '%s="%s"\n' "${da_assignment%%=*}" "${da_value//\"/\\\"}" >>"$da_file"
  done
}
