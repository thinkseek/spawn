#!/bin/sh
# Black box (objective 131) [performance]: an owner's gated push that changes
# the UI's dependencies is judged on them. The integrator installs the
# candidate's own set into the judging's tree before the gate, from the
# host's pnpm store and offline. A set the store lacks is a no-verdict that
# says so, and the gate never starts.
#
# The problem (K2, requests 148307 and 148311): an owner judging's tree gets
# the installed controller's web/node_modules only when its .pnpm/lock.yaml
# is the candidate's web/pnpm-lock.yaml. Any lockfile change left the tree
# without node_modules, and a check that builds the UI could not be judged:
# no verdict, again and again.
#
# The world: a source whose web/ depends on a made-up package (made-up-a)
# from a registry of this run's own on 127.0.0.1, which fills this run's
# pnpm store, makes the lockfiles and stops before anything is judged. The controller ($WORK/ctl, whose rsi/scenarios is the
# mission's gate) has that set installed. A gated mission runs a recording
# gate (SPAWN_GATE), which notes what the tree's web/node_modules holds when
# it starts. pnpm's store is this run's scratch HOME's, never the host's, and
# nothing reaches the network.
#   (1) the owner pushes (gated) a candidate whose lockfile adds made-up-b,
#       which the store holds. One
#       `spawn integrate --once`: the gate runs once and finds web/node_modules
#       a real directory holding made-up-a and made-up-b, whose
#       .pnpm/lock.yaml is the candidate's web/pnpm-lock.yaml, with no file
#       sharing an inode with the store (a candidate's scenario cannot write
#       into the host's store); the push is judged and lands (the source's
#       main holds the candidate).
#   (2) the controller's own node_modules is untouched (no made-up-b).
#   (3) the owner pushes (gated) a candidate whose lockfile names
#       made-up-absent@1.0.0 from the registry, which no store here holds. One
#       pass: the gate does not run; the request's source.push_no_verdict says
#       "dependencies not in the store"; main is unchanged.
#   (4) a second pass gives the second no-verdict for the same reason: the
#       request is refused (verdict no_verdict), and still no gate runs.
#   (5)-(8) a candidate's web/ that could run code in pnpm or write outside
#       its tree (rev-g's F1 on R1), each pushed gated and given two passes:
#       (5) a web/.pnpmfile.cjs, (6) `pnpmfile:` in web/pnpm-workspace.yaml,
#       (7) `modulesDir: ../../outside-modules` there, (8) web/ itself a
#       symbolic link to a directory outside the tree. For each: the hook's
#       marker is never written, nothing named outside-modules exists under
#       the run, the link's target gets no node_modules, no gate runs, the
#       no-verdict begins "the candidate's web/ holds pnpm configuration", and
#       the second pass refuses the request.
#   (9)-(10) a candidate whose dependencies name a directory of the host's
#       (a stand-in for ~/.config holding host-secret-marker) (rev-g's F2 on
#       R2), pushed gated, two passes each: (9) package.json's
#       `"secretpkg": "file:<that directory>"` with the lockfile pnpm makes
#       for it; (10) package.json of the registry's made-up-a and made-up-b,
#       its lockfile's made-up-b `version: link:<that directory>`. For each:
#       the same no-verdict, naming the dependency or the lockfile's value,
#       no gate, the second pass refuses, and the marker is copied nowhere
#       and nothing links to the directory.
#   (11) as (10), with the lockfile's value spelled through a YAML
#       double-quoted escape (`version: "\x6cink:<that directory>"`, which
#       pnpm decodes; rev-g's F3 on R3): the same, naming "a backslash or a
#       double quote".
#
# Exit status: 0 every check passed; 1 an assertion failed (the behaviour is
# missing or wrong); 2 a harness or infrastructure failure; 6 this host
# cannot judge it (no pnpm). Any unexpected exit is reported as 2.

set -eu

asserted=""
harness() {
    echo "131: harness: $*" >&2
    exit 2
}

[ -n "${SPAWN_BIN-}" ] && [ -x "$SPAWN_BIN" ] || harness "SPAWN_BIN is not an executable"
[ -n "${SPAWN_TEST_DATABASE_URL-}" ] || harness "SPAWN_TEST_DATABASE_URL is required"

controller=$(cd "$(dirname "$0")/../.." && pwd)
[ -f "$controller/scripts/blackbox/assertions.sh" ] ||
    harness "no $controller/scripts/blackbox/assertions.sh"
for tool in psql jq od git sed grep curl cut; do
    command -v "$tool" >/dev/null 2>&1 || harness "$tool is not installed"
done

WORK=$(mktemp -d) || harness "mktemp failed"
trap 'rm -rf "$WORK"' EXIT
real_home=$(getent passwd "$(id -u)" | cut -d: -f6)
run="$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SPAWN_DB_NAME="bb_131_modules_$run"
SPAWN_PSQL=psql
export WORK SPAWN_DB_NAME SPAWN_PSQL
HOME=$WORK/home
SPAWN_CLAUDE_PROJECTS=$WORK/home/projects
export HOME SPAWN_CLAUDE_PROJECTS
mkdir -p "$SPAWN_CLAUDE_PROJECTS" || harness "mkdir $SPAWN_CLAUDE_PROJECTS failed"
[ "${HOME#"$WORK"/}" != "$HOME" ] || harness "HOME is not a scratch directory under WORK: $HOME"
[ -z "$real_home" ] || case $HOME/ in "$real_home"/*) harness "HOME $HOME is in the real home $real_home" ;; esac
XDG_RUNTIME_DIR=$WORK/runtime
mkdir -m 700 "$XDG_RUNTIME_DIR"
export XDG_RUNTIME_DIR

# shellcheck source=scripts/pg-url.sh
. "$controller/scripts/pg-url.sh"
pg_url_export "$SPAWN_TEST_DATABASE_URL" ||
    harness "SPAWN_TEST_DATABASE_URL is not a single-host postgres:// URL"
server_url=$(pg_url_server "$SPAWN_TEST_DATABASE_URL")

database_made=""
: >"$WORK/pids"
cleanup() {
    while read -r pid; do
        kill -KILL "$pid" 2>/dev/null || true
    done <"$WORK/pids"
    if [ -n "$database_made" ]; then
        sh "$controller/scripts/blackbox/scenario-db.sh" drop "$SPAWN_DB_NAME" >/dev/null 2>&1 || true
    fi
    rm -rf "$WORK"
}
finish() {
    rc=$?
    trap - EXIT
    set +e
    if [ "$rc" -ne 0 ]; then
        echo "131: the gate runs:" >&2
        cat "$WORK/judged.log" >&2 2>/dev/null || true
        echo "131: the integrator's last lines:" >&2
        grep -v '^| ' "$WORK/integrate.log" 2>/dev/null | tail -n 20 >&2 || true
    fi
    cleanup
    if [ "$rc" -eq 0 ]; then exit 0; fi
    if [ "$rc" -eq 1 ] && [ -n "$asserted" ]; then exit 1; fi
    [ "$rc" -eq 2 ] || echo "131: harness: unexpected exit $rc" >&2
    exit 2
}
trap finish EXIT
trap 'exit 130' INT TERM

sh "$controller/scripts/blackbox/scenario-db.sh" sweep "${SPAWN_DB_NAME%"$run"}" >/dev/null 2>&1 || true
psql -d postgres -tA -c "create database $SPAWN_DB_NAME" >/dev/null 2>&1 ||
    harness "cannot create the scenario database"
database_made=1

. "$controller/scripts/blackbox/assertions.sh"

: >"$WORK/failures"
check_failed() {
    if ! psql -d "$SPAWN_DB_NAME" -tA -c 'select 1' >/dev/null 2>&1; then
        harness "the database is unreachable (while checking: $*)"
    fi
    echo "blackbox: $*" >&2
    echo "$*" >>"$WORK/failures"
}

for name in SPAWN_PROGRAM SPAWN_JUDGE_URL SPAWN_JUDGE_MODEL SPAWN_MODEL_RELAY SPAWN_GATE \
    SPAWN_DISPATCH SPAWN_SUPERVISOR CLAUDE_CONFIG_DIR SPAWN_SERVE_TOKEN SPAWN_SERVE_DATABASE_URL \
    SPAWN_GATE_MIN_FREE_GB SPAWN_GATE_JOBS SPAWN_GATE_WORKER_CORES SPAWN_GATE_MIN_FREE_MEMORY_MB \
    SPAWN_INTEGRATE_GATES SPAWN_TEST_DATABASE_LOG SPAWN_INTEGRATE_DRAIN_SECS SPAWN_GATE_ENV_FILE \
    SPAWN_INTEGRATE_WARM SPAWN_INTEGRATE_SHADOW SPAWN_GATE_RUNNER; do
    unset "$name" 2>/dev/null || true
done
SPAWN_DATABASE_URL="$server_url/$SPAWN_DB_NAME"
SPAWN_WORKSPACE_ROOT=$WORK/ws
export SPAWN_DATABASE_URL SPAWN_WORKSPACE_ROOT
mkdir -p "$SPAWN_WORKSPACE_ROOT"

tries=0
while :; do
    capture env -u SPAWN_TEST_DATABASE_URL "$SPAWN_BIN" init-db
    [ "$status" -eq 0 ] && break
    tries=$((tries + 1))
    [ "$tries" -lt 5 ] || harness "init-db failed $tries times: $(err)"
    sleep 1
done

note() { echo "131: $*" >&2; }
q() { scalar "$1"; }

# --- pnpm, and a store of this run's own ------------------------------------------------
pnpm_bin=$(command -v pnpm 2>/dev/null || true)
if [ -z "$pnpm_bin" ] && node_bin=$(command -v node 2>/dev/null); then
    candidate_pnpm="$(dirname "$(readlink -f "$node_bin")")/pnpm"
    [ -x "$candidate_pnpm" ] && pnpm_bin=$candidate_pnpm
fi
if [ -z "$pnpm_bin" ]; then
    echo "131: cannot judge here: no pnpm on PATH or beside node" >&2
    trap - EXIT
    cleanup
    exit 6
fi
PATH="$(dirname "$pnpm_bin"):$PATH"
for name in PNPM_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_CONFIG_HOME npm_config_store_dir \
    npm_config_registry NPM_CONFIG_REGISTRY http_proxy https_proxy HTTP_PROXY HTTPS_PROXY; do
    unset "$name" 2>/dev/null || true
done
export PATH
store=$(cd "$WORK" && pnpm store path 2>/dev/null) || harness "pnpm store path failed"
case $store in "$WORK"/*) ;; *) harness "pnpm's store is not this run's ($store)" ;; esac

# made-up packages, from a registry of this run's own on 127.0.0.1 (the
# lockfiles then resolve them by integrity alone, as a real registry's
# packages are). It fills the store and makes every lockfile below, then
# stops: everything judged runs offline.
mkdir -p "$WORK/tools" "$WORK/tarballs"
cat >"$WORK/tools/registry.py" <<'PYEOF'
import base64, hashlib, http.server, json, os, re, sys
root, port_file = sys.argv[1], sys.argv[2]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def send(self, code, body, kind):
        self.send_response(code); self.send_header("Content-Type", kind)
        self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
    def do_GET(self):
        path = self.path.split("?")[0].strip("/")
        m = re.fullmatch(r"([a-z0-9-]+)/-/([a-z0-9.-]+\.tgz)", path)
        if m and os.path.isfile(os.path.join(root, m.group(2))):
            return self.send(200, open(os.path.join(root, m.group(2)), "rb").read(), "application/octet-stream")
        versions = {}
        for f in sorted(os.listdir(root)) if re.fullmatch(r"[a-z0-9-]+", path) else []:
            v = re.fullmatch(re.escape(path) + r"-([0-9]+\.[0-9]+\.[0-9]+)\.tgz", f)
            if not v: continue
            data = open(os.path.join(root, f), "rb").read()
            versions[v.group(1)] = {"name": path, "version": v.group(1), "dist": {
                "tarball": "http://127.0.0.1:%d/%s/-/%s" % (self.server.server_port, path, f),
                "integrity": "sha512-" + base64.b64encode(hashlib.sha512(data).digest()).decode(),
                "shasum": hashlib.sha1(data).hexdigest()}}
        if not versions:
            return self.send(404, b'{"error":"not found"}', "application/json")
        body = {"name": path, "versions": versions, "dist-tags": {"latest": sorted(versions)[-1]}}
        self.send(200, json.dumps(body).encode(), "application/json")
s = http.server.HTTPServer(("127.0.0.1", 0), H)
open(port_file + ".tmp", "w").write(str(s.server_port))
os.rename(port_file + ".tmp", port_file)
s.serve_forever()
PYEOF
pack() { # <name>: $WORK/tarballs/<name>-1.0.0.tgz
    mkdir -p "$WORK/pkg-$1/package"
    printf '{"name":"%s","version":"1.0.0","main":"index.js"}\n' "$1" >"$WORK/pkg-$1/package/package.json"
    printf 'module.exports = "%s";\n' "$1" >"$WORK/pkg-$1/package/index.js"
    tar -czf "$WORK/tarballs/$1-1.0.0.tgz" -C "$WORK/pkg-$1" package
}
pack made-up-a
pack made-up-b
python3 -I "$WORK/tools/registry.py" "$WORK/tarballs" "$WORK/registry.port" >"$WORK/registry.log" 2>&1 &
registry_pid=$!
echo "$registry_pid" >>"$WORK/pids"
tries=0
until [ -s "$WORK/registry.port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 50 ] || harness "the registry did not start: $(head -c 300 "$WORK/registry.log")"
    sleep 0.1
done
printf 'registry=http://127.0.0.1:%s/\n' "$(cat "$WORK/registry.port")" >"$HOME/.npmrc"
# lock <web dir>: its pnpm-lock.yaml for its package.json (the registry is up)
lock() {
    (cd "$1" && pnpm install --lockfile-only --ignore-scripts --reporter=silent) >>"$WORK/pnpm.log" 2>&1 ||
        harness "pnpm could not lock $1: $(tail -n 3 "$WORK/pnpm.log" | tr '\n' ' ')"
}
(cd "$WORK" && pnpm store add made-up-a@1.0.0 made-up-b@1.0.0) >>"$WORK/pnpm.log" 2>&1 ||
    harness "pnpm could not add the made-up packages to the store: $(tail -n 3 "$WORK/pnpm.log" | tr '\n' ' ')"
# The lockfiles of (1), (9) and (10), made now. (9)'s names a directory of
# the host's (a stand-in for ~/.config) by file:.
mkdir -p "$WORK/lock-ab" "$WORK/lock-secret" "$WORK/host-secret"
printf '{"name":"secretpkg","version":"1.0.0"}\n' >"$WORK/host-secret/package.json"
printf 'a credential\n' >"$WORK/host-secret/host-secret-marker"
printf '%s\n' '{"name":"bb-web","private":true,"dependencies":{"made-up-a":"1.0.0","made-up-b":"1.0.0"}}' >"$WORK/lock-ab/package.json"
printf '%s\n' "{\"name\":\"bb-web\",\"private\":true,\"dependencies\":{\"made-up-a\":\"1.0.0\",\"made-up-b\":\"1.0.0\",\"secretpkg\":\"file:$WORK/host-secret\"}}" >"$WORK/lock-secret/package.json"
lock "$WORK/lock-ab"
lock "$WORK/lock-secret"
grep -q 'resolution: {integrity: ' "$WORK/lock-ab/pnpm-lock.yaml" && ! grep -q 'tarball' "$WORK/lock-ab/pnpm-lock.yaml" ||
    harness "the registry's lockfile does not resolve by integrity alone"

# --- the world ---------------------------------------------------------------------------
cat >"$WORK/gitconfig" <<'GITEOF'
[user]
	name = bb
	email = bb@localhost
[init]
	defaultBranch = main
[commit]
	gpgsign = false
GITEOF
GIT_CONFIG_GLOBAL=$WORK/gitconfig
export GIT_CONFIG_GLOBAL

seed=$WORK/seed
mkdir -p "$seed/web" "$seed/rsi/scenarios"
git init -q --initial-branch=main "$seed"
printf '#!/bin/sh\nexit 0\n' >"$seed/rsi/scenarios/check-ok.sh"
printf '# nothing protected\n' >"$seed/rsi/scenarios/protected"
: >"$seed/rsi/scenarios/no-test-database"
printf '%s\n' '{"name":"bb-web","private":true,"dependencies":{"made-up-a":"1.0.0"}}' >"$seed/web/package.json"
lock "$seed/web"
kill "$registry_pid" 2>/dev/null || true
wait "$registry_pid" 2>/dev/null || true
printf 'node_modules\n' >"$seed/.gitignore"
git -C "$seed" add -A
git -C "$seed" commit -qm base
src=$WORK/source.git
git clone -q --bare "$seed" "$src"

# the controller: main's tree, its web/ dependencies installed
git clone -q "$src" "$WORK/ctl"
(cd "$WORK/ctl/web" && pnpm install --frozen-lockfile --offline --ignore-scripts --reporter=silent) >>"$WORK/pnpm.log" 2>&1 ||
    harness "the controller's install failed: $(tail -n 3 "$WORK/pnpm.log" | tr '\n' ' ')"
cmp -s "$WORK/ctl/web/pnpm-lock.yaml" "$WORK/ctl/web/node_modules/.pnpm/lock.yaml" ||
    harness "the controller's node_modules/.pnpm/lock.yaml is not its lockfile"

: >"$WORK/judged.log"
: >"$WORK/integrate.log"
mkdir -p "$WORK/cache"
cat >"$WORK/fake-gate.sh" <<GATEEOF
#!/bin/sh
tree=\$(cd "\$4/../.." && pwd)
m=\$tree/web/node_modules
if [ -d "\$m" ] && [ ! -L "\$m" ]; then
    seen=dir
    [ -f "\$m/made-up-a/index.js" ] && seen="\$seen a"
    [ -f "\$m/made-up-b/index.js" ] && seen="\$seen b"
    cmp -s "\$m/.pnpm/lock.yaml" "\$tree/web/pnpm-lock.yaml" && seen="\$seen lock"
    [ -z "\$(find "\$m" -type f -links +1 -print -quit 2>/dev/null)" ] && seen="\$seen own"
else
    seen=none
fi
names=\$(git --git-dir="\$1" show "\$3:web/package.json" 2>/dev/null | grep -o 'made-up-[a-z]*' | sort -u | tr '\n' ',')
printf '%s %s\n' "\$names" "\$seen" >>'$WORK/judged.log'
printf '{"verdict":"accepted"}\n' >"\$SPAWN_GATE_VERDICT"
echo '{"verdict":"accepted"}'
exit 0
GATEEOF
chmod 755 "$WORK/fake-gate.sh"

capture "$SPAWN_BIN" mission add --text "131 dependencies" --source "$src" --gate "$WORK/ctl/rsi/scenarios" --accept gate
[ "$status" -eq 0 ] || harness "mission add failed: $(err)"

git clone -q "$src" "$WORK/owner"
# push <subject>: the owner's commit, pushed gated; sets commit and request
push() {
    git -C "$WORK/owner" add -A
    git -C "$WORK/owner" commit -qm "$1"
    commit=$(git -C "$WORK/owner" rev-parse HEAD)
    capture sh -c 'cd "$1" && shift && exec "$@"' sh "$WORK/owner" "$SPAWN_BIN" mission push --source "$src" --commit "$commit" --gated --reason "$1"
    [ "$status" -eq 0 ] || harness "mission push ($1) failed: $(err)"
    request=$(jq -r '.request // empty' "$WORK/out.txt")
    [ -n "$request" ] || harness "mission push ($1) printed no request: $(head -c 300 "$WORK/out.txt")"
}
integrate() {
    capture env -u DBUS_SESSION_BUS_ADDRESS -u SPAWN_TEST_DATABASE_URL SPAWN_GATE="$WORK/fake-gate.sh" \
        XDG_CACHE_HOME="$WORK/cache" SPAWN_GATE_MIN_FREE_GB=0 SPAWN_GATE_MIN_FREE_MEMORY_MB=1 \
        SPAWN_GATE_JOBS=1 SPAWN_GATE_WORKER_CORES=0 SPAWN_INTEGRATE_WARM=0 \
        "$SPAWN_BIN" integrate --root "$WORK/integrator" --once
    cat "$WORK/out.txt" "$WORK/err.txt" >>"$WORK/integrate.log"
    [ "$status" -eq 0 ] || harness "integrate --once exited $status: $(err | tail -n 3 | tr '\n' ' ')"
}
mkdir -p "$WORK/integrator"
record_of() { # <type> <request>: the latest such record's JSON
    q "select kind -> 'detail' -> 'record' from spawn_events where kind -> 'detail' -> 'record' ->> 'type' = '$1' and (kind -> 'detail' -> 'record' ->> 'request')::bigint = $2 order by id desc limit 1"
}

# --- (1) a lockfile that adds a package pnpm can store offline --------------------------
cp "$WORK/lock-ab/package.json" "$WORK/lock-ab/pnpm-lock.yaml" "$WORK/owner/web/"
push "owner: made-up-b"
c1=$commit
integrate
runs=$(grep -c "^made-up-a,made-up-b, " "$WORK/judged.log" || true)
seen=$(grep "^made-up-a,made-up-b, " "$WORK/judged.log" | head -n 1 | cut -d' ' -f2-)
[ "$runs" = 1 ] || check_failed "(1) the gate ran $runs times on the first candidate, not once"
[ "$seen" = "dir a b lock own" ] ||
    check_failed "(1) at the gate's start the tree's web/node_modules was [$seen], not [dir a b lock own] (a real directory with made-up-a and made-up-b, installed from the candidate's lockfile, no file sharing an inode with the store)"
git --git-dir="$src" merge-base --is-ancestor "$c1" main 2>/dev/null ||
    check_failed "(1) the first candidate was not judged and landed: the source's main does not hold it"
note "(1) checked: a changed lockfile is installed and judged"

# --- (2) the controller's own set ---------------------------------------------------------
[ -f "$WORK/ctl/web/node_modules/made-up-a/index.js" ] && [ ! -e "$WORK/ctl/web/node_modules/made-up-b" ] ||
    check_failed "(2) the controller's web/node_modules changed"
note "(2) checked: the controller's set is untouched"

# --- (3) a lockfile naming a package no store here holds -----------------------------------
main_before=$(git --git-dir="$src" rev-parse main)
python3 -I - "$WORK/owner/web" <<'PY' || harness "could not write the absent lockfile"
import base64, os, sys
web = sys.argv[1]
open(os.path.join(web, "package.json"), "w").write(
    '{"name":"bb-web","private":true,"dependencies":{"made-up-absent":"1.0.0"}}\n')
integrity = "sha512-" + base64.b64encode(os.urandom(64)).decode()
open(os.path.join(web, "pnpm-lock.yaml"), "w").write(f"""lockfileVersion: '9.0'

settings:
  autoInstallPeers: true
  excludeLinksFromLockfile: false

importers:

  .:
    dependencies:
      made-up-absent:
        specifier: 1.0.0
        version: 1.0.0

packages:

  made-up-absent@1.0.0:
    resolution: {{integrity: {integrity}}}

snapshots:

  made-up-absent@1.0.0: {{}}
""")
PY
push "owner: made-up-absent"
c3=$commit
r3=$request
integrate
runs=$(grep -c "^made-up-absent, " "$WORK/judged.log" || true)
[ "$runs" = 0 ] || check_failed "(3) the gate ran $runs times on a candidate whose dependencies no store holds"
reason=$(record_of source.push_no_verdict "$r3" | jq -r '.reason // ""' 2>/dev/null)
case $reason in
"dependencies not in the store"*) ;;
*) check_failed "(3) request $r3's no verdict does not say \"dependencies not in the store\": [$(printf '%s' "$reason" | head -c 200)]" ;;
esac
[ "$(git --git-dir="$src" rev-parse main)" = "$main_before" ] || check_failed "(3) main moved"
note "(3) checked: dependencies not in the store are a named no verdict"

# --- (4) the same no verdict again: refused --------------------------------------------------
integrate
runs=$(grep -c "^made-up-absent, " "$WORK/judged.log" || true)
[ "$runs" = 0 ] || check_failed "(4) the gate ran $runs times on the second pass"
verdict=$(record_of source.push_refused "$r3" | jq -r '.verdict // ""' 2>/dev/null)
[ "$verdict" = no_verdict ] || check_failed "(4) after a second no verdict for the same reason, request $r3 is not refused with verdict no_verdict: [$verdict]"
[ "$(git --git-dir="$src" rev-parse main)" = "$main_before" ] || check_failed "(4) main moved"
note "(4) checked: the second no verdict refuses the request"

# --- (5)-(8) web/ configuration that could run code or write outside ---------------------
# fresh <subject>: the owner's clone back at the source's main
fresh() {
    git -C "$WORK/owner" fetch -q origin
    git -C "$WORK/owner" reset -q --hard origin/main
    git -C "$WORK/owner" clean -qfdx
}
hook() { # <file>: JavaScript that would leave a marker with the environment's names
    printf 'require("fs").writeFileSync(%s, Object.keys(process.env).join(" "));\nmodule.exports = {};\n' \
        "\"$WORK/hook-ran\"" >"$1"
}
refused_config() { # <case> <words the reason holds>
    integrate
    integrate
    runs=$(grep -c "^made-up-a,made-up-b, " "$WORK/judged.log" || true)
    [ "$runs" = 1 ] || check_failed "($1) a gate ran on the candidate (made-up-a,made-up-b runs: $runs)"
    [ ! -e "$WORK/hook-ran" ] || check_failed "($1) the candidate's pnpm hook ran (it saw: $(head -c 200 "$WORK/hook-ran"))"
    [ -z "$(find "$WORK" -name outside-modules -print -quit 2>/dev/null)" ] || check_failed "($1) pnpm wrote outside-modules outside the tree"
    reason=$(record_of source.push_no_verdict "$request" | jq -r '.reason // ""' 2>/dev/null)
    case $reason in
    "the candidate's web/ holds pnpm configuration"*"$2"*) ;;
    *) check_failed "($1) request $request's no verdict is not the web/ configuration refusal naming [$2]: [$(printf '%s' "$reason" | head -c 300)]" ;;
    esac
    [ "$(record_of source.push_refused "$request" | jq -r '.verdict // ""' 2>/dev/null)" = no_verdict ] ||
        check_failed "($1) the second pass did not refuse request $request"
    [ "$(git --git-dir="$src" rev-parse main)" = "$main_before" ] || check_failed "($1) main moved"
    note "($1) checked: refused before pnpm ($2)"
}
main_before=$(git --git-dir="$src" rev-parse main)
fresh
hook "$WORK/owner/web/.pnpmfile.cjs"
push "owner: a pnpmfile"
refused_config 5 "web has .pnpmfile.cjs"

fresh
hook "$WORK/owner/web/hook.cjs"
printf 'minimumReleaseAgeExclude: []\npnpmfile: hook.cjs\n' >"$WORK/owner/web/pnpm-workspace.yaml"
push "owner: a workspace pnpmfile"
refused_config 6 "sets pnpmfile"

fresh
printf 'modulesDir: ../../outside-modules\n' >"$WORK/owner/web/pnpm-workspace.yaml"
push "owner: an outside modulesDir"
refused_config 7 "sets modulesDir"

fresh
mkdir -p "$WORK/outside-web"
cp -R "$WORK/owner/web/." "$WORK/outside-web/"
rm -rf "$WORK/owner/web"
ln -s "$WORK/outside-web" "$WORK/owner/web"
push "owner: web as a link"
refused_config 8 "web is a symbolic link"
[ ! -e "$WORK/outside-web/node_modules" ] || check_failed "(8) the link's target got a node_modules"

# --- (9)-(10) a dependency on a directory of the host's ------------------------------------
copied() { # <case>: the host directory's marker is nowhere but in it
    [ -z "$(find "$WORK" -name host-secret-marker ! -path "$WORK/host-secret/*" -print -quit 2>/dev/null)" ] ||
        check_failed "($1) the host directory's file was copied: $(find "$WORK" -name host-secret-marker ! -path "$WORK/host-secret/*" | head -n 1)"
    [ -z "$(find "$WORK" -type l -lname "$WORK/host-secret*" ! -path "$WORK/lock-*" -print -quit 2>/dev/null)" ] ||
        check_failed "($1) a link into the host directory was made"
}
fresh
# An earlier case that landed (a base without the refusal lands (8)) leaves
# main's web/ no directory: (9) to (11) cannot be planted then.
if [ -L "$WORK/owner/web" ] || [ ! -d "$WORK/owner/web" ]; then
    check_failed "(9)-(11) not run: main's web/ is no directory (an earlier case landed)"
else
    fresh
    cp "$WORK/lock-secret/package.json" "$WORK/lock-secret/pnpm-lock.yaml" "$WORK/owner/web/"
    push "owner: a file: dependency on a host directory"
    refused_config 9 "a dependency that is not a registry range"
    copied 9

    fresh
    cp "$WORK/lock-ab/package.json" "$WORK/lock-ab/pnpm-lock.yaml" "$WORK/owner/web/"
    python3 -I - "$WORK/owner/web/pnpm-lock.yaml" "$WORK/host-secret" <<'PY' || check_failed "(10) could not plant the link: in the lockfile"
import sys
path, target = sys.argv[1], sys.argv[2]
text = open(path).read()
old = "      made-up-b:\n        specifier: 1.0.0\n        version: 1.0.0\n"
assert text.count(old) == 1, text
open(path, "w").write(text.replace(old, "      made-up-b:\n        specifier: 1.0.0\n        version: link:" + target + "\n"))
PY
    push "owner: a lockfile linking a host directory"
    refused_config 10 "a value that is not the registry's (version)"
    copied 10

    fresh
    cp "$WORK/lock-ab/package.json" "$WORK/lock-ab/pnpm-lock.yaml" "$WORK/owner/web/"
    python3 -I - "$WORK/owner/web/pnpm-lock.yaml" "$WORK/host-secret" <<'PY' || check_failed "(11) could not plant the escaped link: in the lockfile"
import sys
path, target = sys.argv[1], sys.argv[2]
text = open(path).read()
old = "      made-up-b:\n        specifier: 1.0.0\n        version: 1.0.0\n"
assert text.count(old) == 1, text
open(path, "w").write(text.replace(old, "      made-up-b:\n        specifier: 1.0.0\n        version: \"\\x6cink:" + target + "\"\n"))
PY
    push "owner: a lockfile linking a host directory through a YAML escape"
    refused_config 11 "a backslash or a double quote"
    copied 11
fi

if [ -s "$WORK/failures" ]; then
    asserted=1
    echo "131: $(wc -l <"$WORK/failures" | tr -d ' ') check(s) failed" >&2
    exit 1
fi
echo "131: an owner's push that changes the UI's dependencies is judged on them, installed offline"
exit 0
