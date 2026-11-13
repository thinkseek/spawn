#!/bin/sh
# test-web-install.sh: what scripts/web-install.sh installs and refuses.
#
# Real pnpm (on PATH, or beside node), offline, from a store under this
# run's scratch HOME, with made-up packages from a registry of this run's
# own on 127.0.0.1 (it fills the store and makes the lockfiles, then
# stops); never the host's store or network. A stub pnpm records the command line and the
# environment names the helper gives it.
#   - today's good trees install: a web/ with this tree's own
#     web/pnpm-workspace.yaml (--strict=web), and a workspace with Supply's
#     root pnpm-workspace.yaml and package.json fields as they are today
#     (--strict=supply), its packageImportMethod: copy and a hardlink
#     variant both giving files of the tree's own (one link each);
#   - the four trees the owner gate refuses (rsi/scenarios/131.sh (5)-(8):
#     a .pnpmfile.cjs, a workspace pnpmfile, modulesDir outside, web/ a
#     symbolic link) are refused before pnpm runs: no marker, nothing
#     outside, no pnpm call;
#   - a dependency on a directory of the host's (file:, link:, a lockfile
#     link:, a link that leaves the tree through a symbolic link) is
#     refused (rev-g's F2 on 131 R2), and so is a lockfile holding a
#     backslash or a double quote (YAML escapes; F3 on R3);
#   - one tree per other rule, each refused alone (exit 3), and the usage.
# The helper runs with HOME and the store in scratch; this test removes
# only its own scratch directory.
set -u
here=$(cd "$(dirname "$0")" && pwd)
helper=$here/web-install.sh
failed=0
ok() { echo "ok   $1"; }
fail() {
    echo "FAIL $1"
    failed=$((failed + 1))
}
pnpm_dir=
if command -v pnpm >/dev/null 2>&1; then
    pnpm_dir=$(dirname "$(command -v pnpm)")
elif command -v node >/dev/null 2>&1 && [ -x "$(dirname "$(command -v node)")/pnpm" ]; then
    pnpm_dir=$(dirname "$(command -v node)")
fi
[ -n "$pnpm_dir" ] || { echo "test-web-install: no pnpm on PATH or beside node" >&2; exit 2; }
WORK=$(mktemp -d "${TMPDIR:-/tmp}/test-web-install.XXXXXX") || exit 2
trap 'rm -rf "$WORK"' EXIT
case $WORK in /*/test-web-install.*) ;; *) echo "test-web-install: scratch $WORK" >&2; exit 2 ;; esac
mkdir "$WORK/home" "$WORK/outside"
# Nothing of the caller's pnpm or npm configuration reaches the runs.
export HOME="$WORK/home" PATH="$pnpm_dir:$PATH"
unset PNPM_HOME XDG_DATA_HOME XDG_CACHE_HOME XDG_STATE_HOME XDG_CONFIG_HOME npm_config_store_dir 2>/dev/null
store=$(cd "$WORK" && env -i PATH="$PATH" HOME="$HOME" pnpm store path 2>/dev/null)
case $store in "$WORK"/*) ;; *) echo "test-web-install: pnpm's store is not this run's ($store)" >&2; exit 2 ;; esac

# The stub: records "$*" and the sorted environment names, then installs
# nothing but node_modules/.pnpm/lock.yaml (the lockfile).
cat >"$WORK/stub-pnpm" <<EOF
#!/bin/sh
printf '%s\n' "\$*" >>"$WORK/stub.args"
env | sed 's/=.*//' | grep -vx 'PWD\|OLDPWD\|SHLVL\|_' | sort | tr '\n' ' ' >>"$WORK/stub.env"
echo >>"$WORK/stub.env"
mkdir -p node_modules/.pnpm && cp pnpm-lock.yaml node_modules/.pnpm/lock.yaml
EOF
chmod +x "$WORK/stub-pnpm"

# The registry: made-up-a and made-up-b, by integrity.
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
# pnpm itself at the host's version, as Supply's packageManager pins it: a
# made-up one whose bin leaves a marker, so a switch to it would show.
pnpm_version=$(pnpm --version) || exit 2
mkdir -p "$WORK/pkg-pnpm/package/bin"
printf '{"name":"pnpm","version":"%s","bin":{"pnpm":"bin/pnpm.cjs"}}\n' "$pnpm_version" >"$WORK/pkg-pnpm/package/package.json"
printf '#!/usr/bin/env node\nrequire("fs").writeFileSync("%s", "ran");\n' "$WORK/pnpm-switched" >"$WORK/pkg-pnpm/package/bin/pnpm.cjs"
tar -czf "$WORK/tarballs/pnpm-$pnpm_version.tgz" -C "$WORK/pkg-pnpm" package
python3 -I "$WORK/tools/registry.py" "$WORK/tarballs" "$WORK/registry.port" >"$WORK/registry.log" 2>&1 &
registry_pid=$!
trap 'kill "$registry_pid" 2>/dev/null; rm -rf "$WORK"' EXIT
tries=0
until [ -s "$WORK/registry.port" ]; do
    tries=$((tries + 1))
    [ "$tries" -lt 50 ] || { echo "test-web-install: the registry did not start" >&2; exit 2; }
    sleep 0.1
done
printf 'registry=http://127.0.0.1:%s/\n' "$(cat "$WORK/registry.port")" >"$HOME/.npmrc"
# lock <dir>: its pnpm-lock.yaml (the registry is up), made in a copy, so
# nothing but the lockfile comes back.
lock() {
    rm -rf "$WORK/lockwork" && cp -R "$1" "$WORK/lockwork" &&
        (cd "$WORK/lockwork" && env -i PATH="$PATH" HOME="$HOME" LC_ALL=C pnpm install --lockfile-only --config.manage-package-manager-versions=false \
            --ignore-scripts --ignore-pnpmfile --reporter=silent) >"$WORK/lock.log" 2>&1 &&
        cp "$WORK/lockwork/pnpm-lock.yaml" "$1/" || { cat "$WORK/lock.log"; return 1; }
}
(cd "$WORK" && env -i PATH="$PATH" HOME="$HOME" LC_ALL=C pnpm store add made-up-a@1.0.0 made-up-b@1.0.0) >"$WORK/store.log" 2>&1 ||
    { cat "$WORK/store.log"; exit 2; }

# --- today's good web/ tree -------------------------------------------------------------
good_web=$WORK/good-web
mkdir -p "$good_web"
cp "$here/../web/pnpm-workspace.yaml" "$good_web/"
printf '%s\n' '{"name":"bb-web","private":true,"version":"0.0.0","type":"module","scripts":{"build":"true"},"dependencies":{"made-up-a":"1.0.0"}}' >"$good_web/package.json"
lock "$good_web" || { echo "test-web-install: could not lock the good web tree" >&2; exit 2; }
# a web/ that names a directory of the host's (rev-g's F2): its lockfile too
mkdir -p "$WORK/host-secret" "$WORK/file-web"
printf '{"name":"secretpkg","version":"1.0.0"}\n' >"$WORK/host-secret/package.json"
printf 'a credential\n' >"$WORK/host-secret/host-secret-marker"
cp "$good_web/pnpm-workspace.yaml" "$WORK/file-web/"
printf '%s\n' "{\"name\":\"bb-web\",\"private\":true,\"dependencies\":{\"made-up-a\":\"1.0.0\",\"secretpkg\":\"file:$WORK/host-secret\"}}" >"$WORK/file-web/package.json"
lock "$WORK/file-web" || { echo "test-web-install: could not lock the file: web tree" >&2; exit 2; }
# makes <name> <from>: a copy of a good tree to change
copy() { rm -rf "$WORK/$1" && cp -R "$2" "$WORK/$1" && printf '%s' "$WORK/$1"; }

# run <tree> <mode> [stub]: the helper's exit, its stderr in $WORK/err
run() {
    if [ "${3-}" = stub ]; then
        SPAWN_PNPM=$WORK/stub-pnpm sh "$helper" "$1" --offline "--strict=$2" 2>"$WORK/err" >/dev/null
    else
        sh "$helper" "$1" --offline "--strict=$2" 2>"$WORK/err" >/dev/null
    fi
}
# own_files <tree>: "ok" when every file in node_modules has one link (none shares the store's inode)
own_files() {
    if find "$1/node_modules" -type f -links +1 | grep -q .; then echo shared; else echo ok; fi
}

# (1) today's web/: the real pnpm installs it, a copy of the tree's own files, lock equal.
tree=$(copy web1 "$good_web")
if run "$tree" web; then
    [ -f "$tree/node_modules/made-up-a/index.js" ] && cmp -s "$tree/pnpm-lock.yaml" "$tree/node_modules/.pnpm/lock.yaml" &&
        [ "$(own_files "$tree")" = ok ] && ok "(1) today's web/ installs (made-up-a, lock equal, no file shared with the store)" ||
        fail "(1) today's web/ installed, but not made-up-a with the lock and files of its own"
else
    fail "(1) today's web/ was not installed: $(tail -n 3 "$WORK/err")"
fi

# (1b) today's real web/ configuration (this tree's package.json,
# pnpm-lock.yaml and pnpm-workspace.yaml) passes every rule (stub: its
# packages are not in this store).
mkdir -p "$WORK/real-web"
cp "$here/../web/package.json" "$here/../web/pnpm-lock.yaml" "$here/../web/pnpm-workspace.yaml" "$WORK/real-web/"
run "$WORK/real-web" web stub && ok "(1b) this tree's own web/ configuration passes" ||
    fail "(1b) this tree's own web/ configuration was refused: $(cat "$WORK/err")"

# (2) the command line and the environment pnpm gets (stub), the caller's
# npm and pnpm variables set.
tree=$(copy web2 "$good_web")
: >"$WORK/stub.args"
: >"$WORK/stub.env"
if npm_config_registry=http://127.0.0.1:9/ PNPM_HOME=$WORK/outside npm_config_modules_dir=$WORK/outside \
    SPAWN_PNPM=$WORK/stub-pnpm sh "$helper" "$tree" --prefer-offline --strict=web 2>"$WORK/err" >/dev/null; then
    want="install --frozen-lockfile --prefer-offline --ignore-scripts --ignore-pnpmfile --config.modules-dir=node_modules --config.virtual-store-dir=node_modules/.pnpm --config.manage-package-manager-versions=false --pm-on-fail=ignore --config.package-import-method=clone-or-copy --reporter=append-only"
    [ "$(cat "$WORK/stub.args")" = "$want" ] && ok "(2) pnpm's command line is the pinned one" ||
        fail "(2) pnpm's command line was [$(cat "$WORK/stub.args")], not [$want]"
    [ "$(cat "$WORK/stub.env")" = "HOME LC_ALL PATH " ] && ok "(2) pnpm's environment is HOME, LC_ALL and PATH alone" ||
        fail "(2) pnpm's environment was [$(cat "$WORK/stub.env")], not [HOME LC_ALL PATH ]"
else
    fail "(2) the stubbed install failed: $(tail -n 3 "$WORK/err")"
fi

# --- today's Supply root -----------------------------------------------------------------
# Supply's root pnpm-workspace.yaml as it is today (supply-pa main), and its
# package.json fields (the root's; an app's).
supply=$WORK/supply
mkdir -p "$supply/apps/api" "$supply/packages/domain"
cat >"$supply/pnpm-workspace.yaml" <<'EOF'
packages:
  - apps/*
  - packages/*

# pnpm reads its own settings here, not from .npmrc.
# Isolated node_modules (pnpm's default linker) holding real files, copied
# rather than hard-linked from the store: Spawn's gate binds this checkout's
# node_modules into its sandbox, where the store is not visible.
nodeLinker: isolated
packageImportMethod: copy
engineStrict: true
# No dependency runs an install script. esbuild's only checks its binary,
# which its platform package already provides.
allowBuilds:
  esbuild: false
EOF
printf '%s\n' '{"name":"bb-supply","version":"0.0.0","private":true,"description":"d","type":"module","packageManager":"pnpm@'"$pnpm_version"'","engines":{"node":">=20"},"scripts":{"test":"true"},"devDependencies":{}}' >"$supply/package.json"
printf '%s\n' '{"name":"@bb/api","version":"0.0.0","private":true,"description":"d","type":"module","main":"index.js","exports":{".":"./index.js"},"scripts":{"build":"true"},"dependencies":{"made-up-b":"1.0.0","@bb/domain":"workspace:*"},"devDependencies":{}}' >"$supply/apps/api/package.json"
printf '%s\n' '{"name":"@bb/domain","version":"0.0.0","private":true,"description":"d","type":"module","exports":{".":"./index.js"},"scripts":{},"dependencies":{"made-up-a":"1.0.0"}}' >"$supply/packages/domain/package.json"
lock "$supply" || { echo "test-web-install: could not lock the Supply tree" >&2; exit 2; }
grep -q 'version: link:../../packages/domain$' "$supply/pnpm-lock.yaml" ||
    { echo "test-web-install: the Supply lockfile does not link the workspace package" >&2; exit 2; }
# Everything below is offline: the registry stops.
kill "$registry_pid" 2>/dev/null
wait "$registry_pid" 2>/dev/null

# (3) Supply's root installs under --strict=supply, files its own; (4) with
# packageImportMethod: hardlink, the command line's clone-or-copy still wins.
for variant in copy hardlink; do
    tree=$(copy "supply-$variant" "$supply")
    sed -i "s/^packageImportMethod: copy$/packageImportMethod: $variant/" "$tree/pnpm-workspace.yaml"
    check="(3) Supply's root (packageImportMethod: $variant)"
    if run "$tree" supply; then
        [ -f "$tree/apps/api/node_modules/made-up-b/index.js" ] && [ ! -e "$WORK/pnpm-switched" ] &&
            [ "$(readlink -f "$tree/apps/api/node_modules/@bb/domain")" = "$(readlink -f "$tree/packages/domain")" ] && [ "$(own_files "$tree")" = ok ] && [ "$(own_files "$tree/apps/api")" = ok ] &&
            ok "$check installs, no file shared with the store" ||
            fail "$check installed, but not made-up-b and @bb/domain in files of its own, or a pinned pnpm ran"
    else
        fail "$check was not installed: $(tail -n 3 "$WORK/err")"
    fi
done

# --- refusals ----------------------------------------------------------------------------
# refused <check> <tree> <mode> <reason part>: exit 3 naming the reason, no
# pnpm call (stub), no marker, nothing outside.
refused() {
    : >"$WORK/stub.args"
    rm -f "$WORK/marker"
    run "$2" "$3" stub
    code=$?
    if [ "$code" -ne 3 ]; then
        fail "$1: exit $code, not 3 ($(tail -n 2 "$WORK/err"))"
    elif ! grep -qF "$4" "$WORK/err"; then
        fail "$1: refused, but not naming [$4]: $(cat "$WORK/err")"
    elif [ -s "$WORK/stub.args" ] || [ -e "$WORK/marker" ] || [ -n "$(ls -A "$WORK/outside")" ]; then
        fail "$1: refused, but pnpm ran or something was written"
    else
        ok "$1"
    fi
}
pnpmfile="require('fs').writeFileSync('$WORK/marker', 'ran'); module.exports = {};"

# 131.sh's four trees (5)-(8)
tree=$(copy bad5 "$good_web") && printf '%s\n' "$pnpmfile" >"$tree/.pnpmfile.cjs"
refused "(5) a .pnpmfile.cjs" "$tree" web "has .pnpmfile.cjs"
tree=$(copy bad6 "$good_web") && printf '%s\n' "$pnpmfile" >"$tree/hook.cjs" && printf 'pnpmfile: hook.cjs\n' >>"$tree/pnpm-workspace.yaml"
refused "(6) a workspace pnpmfile" "$tree" web "pnpm-workspace.yaml sets pnpmfile"
tree=$(copy bad7 "$good_web") && printf 'modulesDir: %s\n' "$WORK/outside" >>"$tree/pnpm-workspace.yaml"
refused "(7) modulesDir outside" "$tree" web "pnpm-workspace.yaml sets modulesDir"
tree=$(copy bad8-real "$good_web") && ln -s "$tree" "$WORK/bad8"
refused "(8) the directory a symbolic link" "$WORK/bad8" web "it is not a real directory"

# one tree per other rule
tree=$(copy r1 "$good_web") && printf 'registry=http://127.0.0.1:9/\n' >"$tree/.npmrc"
refused "(9) an .npmrc" "$tree" web "has .npmrc"
tree=$(copy r2 "$good_web") && rm "$tree/pnpm-workspace.yaml"
refused "(10) no pnpm-workspace.yaml of its own" "$tree" web "pnpm-workspace.yaml is not a regular file"
tree=$(copy r3 "$good_web") && mv "$tree/pnpm-workspace.yaml" "$tree/ws.yaml" && ln -s ws.yaml "$tree/pnpm-workspace.yaml"
refused "(11) pnpm-workspace.yaml a symbolic link" "$tree" web "pnpm-workspace.yaml is not a regular file"
tree=$(copy r4 "$good_web") && mv "$tree/package.json" "$tree/p.json" && ln -s p.json "$tree/package.json"
refused "(12) package.json a symbolic link" "$tree" web "package.json is not a regular file"
tree=$(copy r5 "$good_web") && mkdir "$WORK/outside-nm" && ln -s "$WORK/outside-nm" "$tree/node_modules"
refused "(13) node_modules a symbolic link" "$tree" web "node_modules is a symbolic link"
rmdir "$WORK/outside-nm"
tree=$(copy r6 "$good_web") && printf '%s\n' '{"name":"bb-web","pnpm":{"overrides":{"made-up-a":"1.0.0"}}}' >"$tree/package.json"
refused "(14) package.json's pnpm field" "$tree" web "package.json has the field pnpm"
tree=$(copy r7 "$good_web") && printf '[]\n' >"$tree/package.json"
refused "(15) package.json not an object" "$tree" web "package.json is not a JSON object"
tree=$(copy r8 "$good_web") && printf 'minimumReleaseAgeExclude:\n\t- x\n' >"$tree/pnpm-workspace.yaml"
refused "(16) a tab indent" "$tree" web "indents with a tab"
tree=$(copy r9 "$good_web") && printf '  minimumReleaseAgeExclude: []\n' >"$tree/pnpm-workspace.yaml"
refused "(17) an indented first setting" "$tree" web "first setting is indented"
tree=$(copy r10 "$good_web") && printf '{modulesDir: x}\n' >"$tree/pnpm-workspace.yaml"
refused "(18) a flow mapping" "$tree" web "pnpm-workspace.yaml sets a key that is not a plain name"
tree=$(copy r11 "$good_web") && printf -- '---\n' >"$tree/pnpm-workspace.yaml"
refused "(19) a line that is no key" "$tree" web "has a line that is not a key"
# the lists are each mode's own
refused "(20) Supply's root under --strict=web" "$(copy r12 "$supply")" web "pnpm-workspace.yaml sets packages"
refused "(21) today's web/ under --strict=supply" "$(copy r13 "$good_web")" supply "pnpm-workspace.yaml sets minimumReleaseAgeExclude"
tree=$(copy r14 "$supply") && printf 'overrides:\n  made-up-b: 1.0.0\n' >>"$tree/pnpm-workspace.yaml"
refused "(22) a key beyond Supply's five" "$tree" supply "pnpm-workspace.yaml sets overrides"
tree=$(copy r15 "$supply") && sed -i 's|^  - packages/\*$|  - ../outside|' "$tree/pnpm-workspace.yaml"
refused "(23) a packages entry outside" "$tree" supply "an entry that is not a plain path inside it"
tree=$(copy r16 "$supply") && sed -i "s|^  - packages/\*$|  - $WORK/outside|" "$tree/pnpm-workspace.yaml"
refused "(24) a packages entry absolute" "$tree" supply "an entry that is not a plain path inside it"
tree=$(copy r17 "$supply") && sed -i 's|^packages:$|packages: [apps/*]|' "$tree/pnpm-workspace.yaml"
refused "(25) packages not a block list" "$tree" supply "packages is not a block list"
tree=$(copy r18 "$supply") && sed -i 's|^  - packages/\*$|  key: value|' "$tree/pnpm-workspace.yaml"
refused "(26) a packages line no entry" "$tree" supply "packages has a line that is not a list entry"
tree=$(copy r19 "$supply") && mkdir -p "$WORK/outside-pkg" && printf '{"name":"x"}\n' >"$WORK/outside-pkg/package.json" &&
    ln -s "$WORK/outside-pkg" "$tree/apps/link"
refused "(27) a workspace package a symbolic link" "$tree" supply "the workspace package apps/link is a symbolic link"
rm -rf "$WORK/outside-pkg"
tree=$(copy r20 "$supply") && mkdir -p "$WORK/outside-apps" && mv "$tree/apps" "$WORK/outside-apps/apps" && ln -s "$WORK/outside-apps/apps" "$tree/apps"
refused "(28) a workspace path a symbolic link" "$tree" supply "the workspace path apps is a symbolic link"
rm -rf "$WORK/outside-apps"
tree=$(copy r21 "$supply") && printf 'registry=http://127.0.0.1:9/\n' >"$tree/apps/api/.npmrc"
refused "(29) a workspace package's .npmrc" "$tree" supply "apps/api has .npmrc"
tree=$(copy r22 "$supply") && printf '%s\n' '{"name":"@bb/api","pnpm":{"overrides":{}}}' >"$tree/apps/api/package.json"
refused "(30) a workspace package's pnpm field" "$tree" supply "apps/api/package.json has the field pnpm"
tree=$(copy r23 "$supply") && printf '%s\n' '{"name":"bb-supply","pnpm":{"overrides":{}}}' >"$tree/package.json"
refused "(31) Supply's root package.json pnpm field" "$tree" supply "package.json has the field pnpm"

# a dependency on a directory of the host's (rev-g's F2 on 131 R2)
copied() { # <check>: no node_modules holds the host directory's marker or a link to it
    # (the trees of (40) plant a link to it outside node_modules, on purpose)
    if [ -n "$(find "$WORK" -path '*/node_modules/*' \( -name host-secret-marker -o -lname "$WORK/host-secret*" \) -print -quit)" ]; then
        fail "$1: the host directory was copied or linked"
    fi
}
refused "(33) a file: dependency on a host directory" "$(copy f1 "$WORK/file-web")" web "a dependency that is not a registry range"
copied "(33)"
tree=$(copy f2 "$good_web") && sed -i "s|\"made-up-a\":\"1.0.0\"|\"made-up-a\":\"link:$WORK/host-secret\"|" "$tree/package.json"
refused "(34) a link: dependency" "$tree" web "a dependency that is not a registry range"
tree=$(copy f3 "$good_web") && sed -i "s|^        version: 1.0.0$|        version: link:$WORK/host-secret|" "$tree/pnpm-lock.yaml"
refused "(35) a lockfile's link: to a host directory (web)" "$tree" web "a value that is not the registry's (version)"
copied "(35)"
tree=$(copy f4 "$good_web") && sed -i "s|^    resolution: {integrity: \(.*\)}$|    resolution: {integrity: \1, tarball: file:$WORK/host-secret}|" "$tree/pnpm-lock.yaml"
refused "(36) a lockfile's tarball: resolution" "$tree" web "a resolution that is not a registry integrity alone"
tree=$(copy f5 "$good_web") && sed -i 's|"made-up-a":"1.0.0"|"made-up-a":"workspace:*"|' "$tree/package.json"
refused "(37) workspace:* under --strict=web" "$tree" web "a dependency that is not a registry range"
tree=$(copy f6 "$supply") && sed -i "s|link:../../packages/domain|link:$WORK/host-secret|" "$tree/pnpm-lock.yaml"
refused "(38) Supply's lockfile linking an absolute path" "$tree" supply "links a workspace package by an absolute path"
tree=$(copy f7 "$supply") && sed -i 's|link:../../packages/domain|link:../../../host-secret|' "$tree/pnpm-lock.yaml" && mkdir -p "$WORK/host-secret"
refused "(39) Supply's lockfile linking outside by .." "$tree" supply "links a directory outside it"
tree=$(copy f8 "$supply") && ln -s "$WORK/host-secret" "$tree/inside-link" && sed -i 's|link:../../packages/domain|link:../../inside-link|' "$tree/pnpm-lock.yaml"
refused "(40) Supply's lockfile linking out through a symbolic link" "$tree" supply "links a directory outside it"
tree=$(copy f9 "$supply") && sed -i 's|"@bb/domain":"workspace:\*"|"@bb/domain":"workspace:^"|' "$tree/apps/api/package.json"
refused "(41) a workspace: specifier other than workspace:*" "$tree" supply "a dependency that is not a registry range"
tree=$(copy f10 "$supply") && sed -i 's|^packages:$|packages:\n  x@link:../outside:\n    resolution: {integrity: sha512-x}|' "$tree/pnpm-lock.yaml"
refused "(42) a package key naming link:" "$tree" supply "names a package that is not the registry's"

# (43) --strict=web: an install whose node_modules/.pnpm/lock.yaml is not
# web/'s lockfile fails (a stub that writes another).
sed 's|cp pnpm-lock.yaml node_modules/.pnpm/lock.yaml|printf other >node_modules/.pnpm/lock.yaml|' "$WORK/stub-pnpm" >"$WORK/stub-other-lock"
chmod +x "$WORK/stub-other-lock"
tree=$(copy l1 "$good_web")
SPAWN_PNPM=$WORK/stub-other-lock sh "$helper" "$tree" --offline --strict=web 2>"$WORK/err" >/dev/null
code=$?
[ "$code" -eq 1 ] && grep -qF "is not $tree/pnpm-lock.yaml after the install" "$WORK/err" &&
    ok "(43) an installed lock.yaml that is not web/'s lockfile fails" ||
    fail "(43) an installed lock.yaml that is not web/'s lockfile: exit $code ($(cat "$WORK/err"))"
# (44) a lockfile's complex key, anchor, alias or tag
tree=$(copy l2 "$good_web") && sed -i 's|^packages:$|packages:\n  ? x@1.0.0\n  : {resolution: {tarball: x}}|' "$tree/pnpm-lock.yaml"
refused "(44) a lockfile's complex key" "$tree" web "a complex key, an anchor, an alias or a tag"

# (50)-(54) a lockfile spelled through YAML double-quoted escapes (rev-g's
# F3 on 131 R3): a value, a package key, the resolution key, a plain quote
tree=$(copy e1 "$good_web") && sed -i "s|^        version: 1.0.0$|        version: \"\\\\x6cink:$WORK/host-secret\"|" "$tree/pnpm-lock.yaml"
refused "(50) an escaped lockfile value" "$tree" web "a backslash or a double quote"
tree=$(copy e2 "$good_web") && sed -i "s|^packages:$|packages:\\n  \"secretpkg@\\\\x66ile:$WORK/host-secret\":\\n    resolution: {integrity: sha512-x}|" "$tree/pnpm-lock.yaml"
refused "(51) an escaped package key" "$tree" web "a backslash or a double quote"
tree=$(copy e3 "$good_web") && sed -i "s|^    resolution: {integrity: \(.*\)}$|    \"r\\\\x65solution\": {directory: $WORK/host-secret, type: directory}|" "$tree/pnpm-lock.yaml"
refused "(52) an escaped resolution key" "$tree" web "a backslash or a double quote"
tree=$(copy e4 "$supply") && sed -i 's|^        version: link:../../packages/domain$|        version: "link:../../packages/domain"|' "$tree/pnpm-lock.yaml"
refused "(53) a double quote alone (supply)" "$tree" supply "a backslash or a double quote"
tree=$(copy e5 "$supply") && sed -i "s|^        version: link:../../packages/domain$|        version: \"\\\\x6cink:$WORK/host-secret\"|" "$tree/pnpm-lock.yaml"
refused "(54) an escaped lockfile value (supply)" "$tree" supply "a backslash or a double quote"
copied "(50)-(54)"

# usage: no mode without a list
for args in "--offline" "--offline --strict=none" "--online --strict=web" "--offline --strict="; do
    # shellcheck disable=SC2086
    SPAWN_PNPM=$WORK/stub-pnpm sh "$helper" "$good_web" $args >/dev/null 2>&1
    code=$?
    [ "$code" -eq 2 ] && ok "(32) usage: [$args] exits 2" || fail "(32) usage: [$args] exited $code, not 2"
done

# --- check-web.sh: web/'s pnpm configuration is in the reviewed set ---------------------
# A copy of rsi/scenarios/check-web.sh with a reviewed file of this test's
# own, run in a candidate tree against a baseline tree (SPAWN_BASELINE_TREE).
# Its dependency part is what is read: the build after it stops at the copy's
# missing node_modules.
mkdir -p "$WORK/cw-ctl/rsi/scenarios" "$WORK/cw-base/web"
cp "$here/../rsi/scenarios/check-web.sh" "$WORK/cw-ctl/rsi/scenarios/"
cp "$good_web/package.json" "$good_web/pnpm-lock.yaml" "$good_web/pnpm-workspace.yaml" "$WORK/cw-base/web/"
cw() { # <check> <tree> <reviewed sha> <words>: check-web.sh's output holds <words>
    printf '%s\n# this test'"'"'s\n' "$3" >"$WORK/cw-ctl/rsi/scenarios/web-dependencies-reviewed"
    (cd "$2" && SPAWN_BASELINE_TREE=$WORK/cw-base sh "$WORK/cw-ctl/rsi/scenarios/check-web.sh") >"$WORK/cw.out" 2>&1
    grep -qF "$4" "$WORK/cw.out" && ok "$1" || fail "$1: check-web.sh said [$(head -n 3 "$WORK/cw.out" | tr '\n' ' ')], not [$4]"
}
cw_tree() { # <name>: a candidate tree holding the good web/ set
    rm -rf "$WORK/$1" && mkdir -p "$WORK/$1/web" &&
        cp "$good_web/package.json" "$good_web/pnpm-lock.yaml" "$good_web/pnpm-workspace.yaml" "$WORK/$1/web/" && printf '%s' "$WORK/$1"
}
tree=$(cw_tree cw1)
cw "(45) the base's own set passes the dependency part" "$tree" none "no node_modules in"
tree=$(cw_tree cw2) && printf 'minimumReleaseAgeExclude:\n  - made-up-a@1.0.0\n' >"$tree/web/pnpm-workspace.yaml"
cw "(46) a changed pnpm-workspace.yaml is a dependency change" "$tree" none "web dependencies changed (web/pnpm-workspace.yaml)"
tree=$(cw_tree cw3) && printf 'registry=http://127.0.0.1:9/\n' >"$tree/web/.npmrc"
cw "(47) a new web/.npmrc is a dependency change" "$tree" none "web dependencies changed (web/.npmrc)"
# the reviewed sha: package.json, the lockfile, then each configuration file there after a line naming it
tree=$(cw_tree cw4) && printf 'minimumReleaseAgeExclude:\n  - made-up-a@1.0.0\n' >"$tree/web/pnpm-workspace.yaml"
reviewed=$({ cat "$tree/web/package.json" "$tree/web/pnpm-lock.yaml"; printf '== web/pnpm-workspace.yaml\n'; cat "$tree/web/pnpm-workspace.yaml"; } | sha256sum | cut -d' ' -f1)
cw "(48) a reviewed set with its pnpm-workspace.yaml passes" "$tree" "$reviewed" "web dependencies: the owner reviewed this set"
printf 'registry=http://127.0.0.1:9/\n' >"$tree/web/.npmrc"
cw "(49) the reviewed set plus an .npmrc is not the reviewed set (refused, naming the first changed file)" "$tree" "$reviewed" "web dependencies changed (web/"

[ "$failed" -eq 0 ] && echo "test-web-install: all passed" || echo "test-web-install: $failed FAIL"
[ "$failed" -eq 0 ]
