#!/bin/sh
# web-install.sh <dir> --offline|--prefer-offline --strict=web|--strict=supply
#
# Installs <dir>'s pnpm dependency set from its frozen lockfile, as the
# owner, with nothing in <dir> able to run code or write outside it. The
# owner's installs call it: deploy/on-landing.sh (Spawn's web/, --strict=web)
# and deploy/controller.sh deps (a mission's root, --strict=supply).
#
# Default deny. pnpm runs code from a pnpmfile (.pnpmfile.cjs, or the one
# pnpm-workspace.yaml names), reads settings from <dir>'s .npmrc and
# pnpm-workspace.yaml (modulesDir and virtualStoreDir can point outside
# <dir>, packages can take in a directory outside it), and reads
# package.json's pnpm field (overrides, patches). So <dir> holds only what
# its list names, and anything else is refused before pnpm runs:
#   - <dir> is a real directory with its own pnpm-workspace.yaml (a
#     workspace file is where pnpm stops looking upward: no file above <dir>
#     is read, its .npmrc included), package.json and pnpm-lock.yaml, each a
#     regular file; node_modules, if there, is a real directory;
#   - no .npmrc, .pnpmfile.cjs, .pnpmfile.mjs or pnpmfile.cjs in <dir> (or,
#     for supply, in a workspace package's directory);
#   - pnpm-workspace.yaml's top-level keys are the list's; a line that is no
#     plain key (a flow mapping, a document marker, an anchor), an indented
#     first setting or a tab indent is refused;
#   - package.json is an object whose fields are the list's (for supply,
#     every workspace package's package.json too).
# --strict=web: the keys and fields of the reviewed web/ (the same rule as
# the owner gate's, integrate/owner.rs web_config_refusal, which installs a
# candidate's web/ before judging it; a change to one is a change to both).
# --strict=supply: exactly the five keys Supply's root has today. Its
# packages entries are plain relative paths (no "..", nothing absolute),
# optionally ending in /*, and no workspace package directory is a
# symbolic link. allowBuilds is inert here: --ignore-scripts runs no build.
# packageImportMethod cannot change the import: the command line's
# clone-or-copy wins over the workspace file (pnpm 12).
# There is no mode without a list: a new key or field in either needs a
# reviewed change to this script.
#
# The install: pnpm with an empty environment but PATH, HOME and LC_ALL
# (no npm_config_* or PNPM_* reaches it; the store is HOME's), the frozen
# lockfile, no lifecycle scripts, no pnpmfile, modules and virtual store
# pinned inside <dir>, no package-manager switching (pnpm 12 records
# package.json's packageManager pin in the lockfile; --pm-on-fail=ignore
# keeps the host's pnpm installing whatever the pin says, never one it
# downloads), and clone-or-copy, so
# no file in the tree shares an inode with the store. --strict=web then
# checks node_modules/.pnpm/lock.yaml is <dir>'s pnpm-lock.yaml.
#
# SPAWN_PNPM names pnpm (default: pnpm on PATH).
# Exit: 0 installed; 1 the install failed; 2 usage; 3 refused.
set -fu
nl='
'

usage() {
    echo "usage: web-install.sh <dir> --offline|--prefer-offline --strict=web|--strict=supply" >&2
    exit 2
}
[ $# -eq 3 ] || usage
dir=$1
case $2 in --offline | --prefer-offline) network=$2 ;; *) usage ;; esac
case $3 in
    --strict=web)
        mode=web
        keys="minimumReleaseAgeExclude"
        fields="name version private type description license scripts dependencies devDependencies optionalDependencies peerDependencies"
        ;;
    --strict=supply)
        mode=supply
        keys="packages nodeLinker packageImportMethod engineStrict allowBuilds"
        fields="name version private type description scripts dependencies devDependencies engines packageManager main exports"
        ;;
    *) usage ;;
esac
# The fields, each as JSON ("name" ...), split here once: the workspace
# packages' loop below splits on lines.
listed_fields=$(printf '"%s" ' $fields)

refuse() {
    echo "web-install: $dir refused: $1" >&2
    exit 3
}
# plain <name>: the name as a reason may say it.
plain() {
    if printf '%s' "$1" | grep -Eqx '[A-Za-z0-9_-]{1,64}'; then
        printf '%s' "$1"
    else
        printf 'a key that is not a plain name'
    fi
}
listed() { # <word> <list>
    case " $2 " in *" $1 "*) return 0 ;; esac
    return 1
}
# regular <path> <shown>: refused unless a regular file (no symbolic link).
regular() {
    [ ! -L "$1" ] && [ -f "$1" ] || refuse "$2 is not a regular file"
}
# no_config <dir> <shown>: refused if <dir> holds .npmrc or a pnpmfile.
no_config() {
    for file in .npmrc .pnpmfile.cjs .pnpmfile.mjs pnpmfile.cjs; do
        [ ! -e "$1/$file" ] && [ ! -L "$1/$file" ] || refuse "$2 has $file"
    done
}
# check_fields <package.json> <shown>: refused unless an object of the list's fields.
check_fields() {
    jq -e 'type == "object"' "$1" >/dev/null 2>&1 || refuse "$2 is not a JSON object"
    for field in $(jq -r 'keys[] | @json' "$1"); do
        name=$(printf '%s' "$field" | jq -r .)
        listed "$field" "$listed_fields" || refuse "$2 has the field $(plain "$name")"
    done
    # Every dependency is the registry's: a range, version or dist-tag (no
    # ":" or "/"), so no file:, link:, git, URL, alias or path names a
    # directory of the host for pnpm to copy or link in; for supply,
    # workspace:* too (a package of the workspace, whose paths are checked).
    bad=$(jq -r --arg mode "$mode" '
        [.dependencies, .devDependencies, .optionalDependencies, .peerDependencies][]
        | select(. != null)
        | if type != "object" then "a dependency field that is not an object"
          else to_entries[]
            | select(((.value | type) != "string")
                or (((.value | test("^[0-9A-Za-z.^~<>=|* +-]+$")) | not)
                    and (($mode != "supply") or (.value != "workspace:*"))))
            | "a dependency that is not a registry range (\(.key | @json))"
          end' "$1" | head -n 1)
    [ -z "$bad" ] || refuse "$2 has $(printf '%s' "$bad" | tr -cd 'A-Za-z0-9 ()@/._:"-' | cut -c1-120)"
}

# check_lock: pnpm-lock.yaml resolves every package from the registry by
# integrity: each resolution is {integrity: ...} alone (no tarball:,
# directory: or git), and no value or key is a file:, link:, git, URL or
# portal: one. For supply, an importer's `version: link:<path>` (a
# workspace:* dependency) is kept when <path>, from the importer's
# directory, is a directory inside <dir> once every symbolic link is
# resolved. A backslash or double quote anywhere is refused: a YAML
# double-quoted scalar can spell any key or value with escapes
# ("\x66ile:", "r\x65solution"), which pnpm decodes and no rule here reads
# (rev-g's F3 on 131 R3); pnpm writes neither.
check_lock() {
    found=$(awk -v mode="$mode" '
        function fail(why) { print "REFUSE " why; exit }
        /[\\"]/ { fail("pnpm-lock.yaml has a backslash or a double quote (pnpm writes neither)") }
        /^[^ #]/ { section = $0; sub(/:.*/, "", section) }
        section == "importers" && /^  [^ ]/ {
            importer = $0; sub(/^  /, "", importer); sub(/:[ ]*$/, "", importer)
            gsub(/^'"'"'|'"'"'$/, "", importer)
        }
        /^[ ]*\?|<<[ ]*:|(^|[ :,\[{-])[&*!][A-Za-z!]/ {
            fail("pnpm-lock.yaml has a complex key, an anchor, an alias or a tag")
        }
        /(^|[ {,])["'"'"']?resolution["'"'"']?[ ]*:/ {
            if ($0 !~ /^ +resolution: \{integrity: [A-Za-z0-9+\/=-]+\}$/)
                fail("pnpm-lock.yaml has a resolution that is not a registry integrity alone")
            next
        }
        /@((file|link|portal|workspace|git|github|gitlab|bitbucket|https?|npm):|git\+)/ {
            fail("pnpm-lock.yaml names a package that is not the registry'"'"'s")
        }
        {
            line = $0
            if (line !~ /^ *[^ :][^:]*: /) next
            key = line; sub(/^ */, "", key); sub(/:.*/, "", key)
            value = line; sub(/^ *[^ :][^:]*: */, "", value)
            gsub(/^['"'"'"]|['"'"'"]$/, "", value)
            if (value !~ /^((file|link|portal|workspace|git|github|gitlab|bitbucket|https?|npm):|git\+)/) next
            if (mode == "supply" && section == "importers" && key == "specifier" && value == "workspace:*") next
            if (mode == "supply" && section == "importers" && key == "version" && value ~ /^link:[^ ]+$/) {
                sub(/^link:/, "", value); print "LINK " importer " " value; next
            }
            fail("pnpm-lock.yaml has a value that is not the registry'"'"'s (" key ")")
        }
    ' "$dir/pnpm-lock.yaml")
    last=$(printf '%s\n' "$found" | tail -n 1)
    case $last in "REFUSE "*) refuse "${last#REFUSE }" ;; esac
    real_dir=$(realpath -e "$dir") || refuse "it cannot be resolved"
    IFS=$nl
    for link in $found; do
        IFS=' '
        set -- $link
        IFS=$nl
        case $3 in /*) refuse "pnpm-lock.yaml links a workspace package by an absolute path" ;; esac
        target=$(realpath -e "$dir/$2/$3" 2>/dev/null) ||
            refuse "pnpm-lock.yaml links a workspace package that is not there ($3)"
        case $target in
            "$real_dir"/*) [ -d "$target" ] || refuse "pnpm-lock.yaml links a workspace package that is no directory ($3)" ;;
            *) refuse "pnpm-lock.yaml links a directory outside it ($3)" ;;
        esac
    done
    unset IFS
}

[ -d "$dir" ] && [ ! -L "$dir" ] || refuse "it is not a real directory"
no_config "$dir" "$(basename "$dir")"
regular "$dir/pnpm-workspace.yaml" pnpm-workspace.yaml
regular "$dir/package.json" package.json
regular "$dir/pnpm-lock.yaml" pnpm-lock.yaml
[ ! -L "$dir/node_modules" ] || refuse "node_modules is a symbolic link"

# pnpm-workspace.yaml: top-level keys, and (supply) the packages entries,
# one per line as "packages <entry>".
parsed=$(awk -v keys=" $keys " '
    function fail(why) { print "REFUSE " why; exit }
    /^[ \t]*(#|$)/ { next }
    /^\t/ { fail("pnpm-workspace.yaml indents with a tab") }
    /^ / {
        if (!started) fail("pnpm-workspace.yaml'"'"'s first setting is indented")
        if (key == "packages") {
            if ($0 !~ /^ +- +[^ ]/) fail("pnpm-workspace.yaml'"'"'s packages has a line that is not a list entry")
            entry = $0; sub(/^ +- +/, "", entry); sub(/ +$/, "", entry)
            if (entry ~ /^".*"$/ || entry ~ /^'"'"'.*'"'"'$/) entry = substr(entry, 2, length(entry) - 2)
            print "packages " entry
        }
        next
    }
    {
        started = 1
        i = index($0, ":")
        if (i == 0) fail("pnpm-workspace.yaml has a line that is not a key")
        key = substr($0, 1, i - 1)
        if (index(keys, " " key " ") == 0) { print "KEY " key; exit }
        if (key == "packages" && substr($0, i + 1) !~ /^ *(#.*)?$/) fail("pnpm-workspace.yaml'"'"'s packages is not a block list")
    }
' "$dir/pnpm-workspace.yaml")
# awk stops at the first refusal, its last line, after the entries before it.
last=$(printf '%s\n' "$parsed" | tail -n 1)
case $last in
    "REFUSE "*) refuse "${last#REFUSE }" ;;
    "KEY "*) refuse "pnpm-workspace.yaml sets $(plain "${last#KEY }")" ;;
esac
check_fields "$dir/package.json" package.json

if [ "$mode" = supply ]; then
    entries=$(printf '%s\n' "$parsed" | sed -n 's/^packages //p')
    IFS=$nl
    for entry in $entries; do
        printf '%s' "$entry" | grep -Eqx '[A-Za-z0-9_@][A-Za-z0-9._@-]*(/[A-Za-z0-9_@][A-Za-z0-9._@-]*)*(/\*)?' ||
            refuse "pnpm-workspace.yaml's packages has an entry that is not a plain path inside it"
        base=${entry%/\*}
        # Each path segment of the entry is a real directory, not a link out.
        path=$dir
        IFS=/
        for segment in $base; do
            path=$path/$segment
            [ ! -L "$path" ] || refuse "the workspace path $base is a symbolic link"
        done
        IFS=$nl
        [ -d "$dir/$base" ] || continue
        if [ "$base" = "$entry" ]; then
            packages=$dir/$base
        else
            packages=$(find "$dir/$base" -mindepth 1 -maxdepth 1 \( -type d -o -type l \) | sort)
        fi
        for package in $packages; do
            shown=${package#"$dir"/}
            [ ! -L "$package" ] || refuse "the workspace package $shown is a symbolic link"
            no_config "$package" "$shown"
            [ ! -L "$package/node_modules" ] || refuse "$shown/node_modules is a symbolic link"
            [ -e "$package/package.json" ] || [ -L "$package/package.json" ] || continue
            regular "$package/package.json" "$shown/package.json"
            check_fields "$package/package.json" "$shown/package.json"
        done
    done
    unset IFS
fi
check_lock

pnpm=$(command -v "${SPAWN_PNPM:-pnpm}") || { echo "web-install: ${SPAWN_PNPM:-pnpm} is not on PATH" >&2; exit 1; }
(cd "$dir" && env -i PATH="$PATH" HOME="$HOME" LC_ALL=C "$pnpm" install --frozen-lockfile "$network" \
    --ignore-scripts --ignore-pnpmfile \
    --config.modules-dir=node_modules --config.virtual-store-dir=node_modules/.pnpm \
    --config.manage-package-manager-versions=false --pm-on-fail=ignore \
    --config.package-import-method=clone-or-copy --reporter=append-only) >&2 || { echo "web-install: pnpm install failed in $dir" >&2; exit 1; }
if [ "$mode" = web ] && ! cmp -s "$dir/pnpm-lock.yaml" "$dir/node_modules/.pnpm/lock.yaml"; then
    echo "web-install: $dir/node_modules/.pnpm/lock.yaml is not $dir/pnpm-lock.yaml after the install" >&2
    exit 1
fi
exit 0
