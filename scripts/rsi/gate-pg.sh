# The gate's own PostgreSQL server (GitHub issue #6), sourced by
# scripts/rsi/gate.sh (POSIX sh).
#
# With SPAWN_GATE_PG_IMAGE or SPAWN_GATE_PG_BIN set, each gate runs a
# disposable server of the test server's PostgreSQL instead of using the
# shared SPAWN_TEST_DATABASE_URL: base results are taken on a clean server,
# and nothing candidate code does to its server reaches a later gate. The
# server's programs come from one of:
# - SPAWN_GATE_PG_IMAGE, a docker image (postgres:16-alpine): its root
#   filesystem is exported once (`docker create` + `docker export`, about 2 s
#   and 300 MB) into ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-pg/<image id>/root.
#   Docker is asked for the image's id on every gate (a pulled update is a
#   new id, a new root and a new key) and runs nothing else;
# - SPAWN_GATE_PG_BIN, the directory of the host's own `postgres` and
#   `initdb` of the test server's version (/usr/lib/postgresql/16/bin, the
#   programs a Debian or Ubuntu test cluster runs), for a host without
#   docker. Its id is a hash of the two programs, their version and the
#   locale (LANG) the cluster is made in: a new package is a new id.
# Not both: that is an infrastructure error.
#
# - A cluster is made once per id by those programs' own initdb, into
#   ${XDG_CACHE_HOME:-~/.cache}/spawn/gate-pg/<id>/template: their defaults,
#   UTF8 and the image's locale (or LANG), the superuser `postgres` with a
#   random password (`password`, this uid's only) and scram-sha-256, as the
#   shared server has them (Spawn derives its restricted logins' passwords
#   from the owner's, so the URL carries one), and the database
#   `spawn_test`. One lock per id: two gates never make it twice.
# - Each gate copies the template and its password into its scratch once (a
#   reflink on btrfs), at its first start: the shared one may be removed and
#   remade with a new password while the gate runs, and each server the gate
#   starts must take the password its sandbox was given. Each copies its data
#   from the gate's copy and runs `postgres` in bubblewrap, as this uid: as
#   `/` the image's files, or the host's /usr (and the server's own
#   installation when it is not under /usr) with the loader's cache and
#   nothing else of the host's: not /etc, /home, /var or /run, all
#   read-only; its data directory and socket directory the only writable
#   paths, no network (a socket only), its own PID, IPC and user namespaces. A superuser's `COPY ... TO PROGRAM` runs
#   there, not on the host. It dies with the gate (`--die-with-parent`, and
#   the gate's EXIT trap); the scratch goes with its data.
# - The sandbox reaches it through a hard link to its socket in a directory
#   of the gate's ($work/test-db): the server can replace the path in its own
#   socket directory (a symlink to another socket of the host), never that
#   link, which names the socket it made at start, before any candidate code
#   ran.
# - Sized for the gate's parallel scenarios: max_connections is 100 plus 50
#   per job (SPAWN_GATE_JOBS), shared_buffers the programs' 128MB, and no
#   fsync (the data is thrown away; durability decides no query result).
#
# gate_pg_start <dir> <jobs> [<option>] starts it with its files in <dir>
# (created, under the gate's scratch) and sets gate_pg_pid (bubblewrap's),
# gate_pg_root, gate_pg_socket (the server's own socket path) and
# gate_pg_password, or says why on stderr and returns 1 when the programs or
# their template cannot be prepared (the owner's configuration: a wrong
# SPAWN_GATE_PG_BIN or image) and 2 when the server itself does not start
# (the host's: test_server_never_started in gate.sh). <option> is one more
# `-c name=value` for the server (the suite's seam, SPAWN_GATE_TEST_PG_FAIL). gate_pg names what it runs (the image or the
# directory), gate_pg_kind and gate_pg_id (`image <id>` or `programs <id>`)
# are for the base cache's key. gate_pg_stop kills it.

gate_pg_image=${SPAWN_GATE_PG_IMAGE-}
gate_pg_bin=${SPAWN_GATE_PG_BIN-}
# What the gate's own server runs, as the log names it; empty: none.
gate_pg=${gate_pg_image:-$gate_pg_bin}
gate_pg_pid=""
gate_pg_id=""
gate_pg_kind=""

# gate_pg_bwrap <root|-> <files dir> <data dir> <socket dir|-> <password
# file|-> <command...>: a command of the image whose root is <root> (`-`: the
# host's programs, SPAWN_GATE_PG_BIN), in its own sandbox, with the PATH and
# LANG of <files dir>/env and this uid as postgres (<files dir>/passwd,
# group); the socket directory is /run/postgresql, the password file
# /tmp/pwfile.
gate_pg_bwrap() {
    pg_root=$1 pg_files=$2 pg_data=$3 pg_sockets=$4 pg_pwfile=$5
    shift 5
    set -- --ro-bind "$pg_files/passwd" /etc/passwd --ro-bind "$pg_files/group" /etc/group \
        --dev /dev --proc /proc --bind "$pg_data" /var/lib/postgresql/data \
        --unshare-all --die-with-parent --new-session \
        --clearenv --setenv PATH "$(sed -n 's/^PATH=//p' "$pg_files/env")" \
        --setenv LANG "$(sed -n 's/^LANG=//p' "$pg_files/env")" \
        --setenv PGDATA /var/lib/postgresql/data "$@"
    [ "$pg_sockets" = - ] || set -- --bind "$pg_sockets" /run/postgresql "$@"
    [ "$pg_pwfile" = - ] || set -- --ro-bind "$pg_pwfile" /tmp/pwfile "$@"
    if [ "$pg_root" != - ]; then
        # The root first, and /tmp before what is bound into it.
        bwrap --ro-bind "$pg_root" / --tmpfs /tmp "$@"
        return
    fi
    # The host's programs: /usr, with /bin, /lib and the like as the host
    # has them (links into /usr, or directories), the server's installation
    # when it is elsewhere, and the loader's cache; nothing else of the host.
    for pg_top in /bin /sbin /lib /lib32 /lib64 /libx32; do
        if [ -L "$pg_top" ]; then
            set -- --symlink "$(readlink "$pg_top")" "$pg_top" "$@"
        elif [ -d "$pg_top" ]; then
            set -- --ro-bind "$pg_top" "$pg_top" "$@"
        fi
    done
    case "$gate_pg_prefix/" in
    /usr/*) ;;
    *) set -- --ro-bind "$gate_pg_prefix" "$gate_pg_prefix" "$@" ;;
    esac
    [ ! -f /etc/ld.so.cache ] || set -- --ro-bind /etc/ld.so.cache /etc/ld.so.cache "$@"
    bwrap --ro-bind /usr /usr --tmpfs /tmp "$@"
}

# gate_pg_users <files dir>: the user the server runs as, this uid named
# postgres (initdb looks itself up), beside the image's env.
gate_pg_users() {
    printf 'root:x:0:0:root:/root:/bin/sh\npostgres:x:%s:%s:postgres:/var/lib/postgresql:/bin/sh\n' \
        "$(id -u)" "$(id -g)" >"$1/passwd"
    printf 'root:x:0:\npostgres:x:%s:\n' "$(id -g)" >"$1/group"
}

# gate_pg_identify: gate_pg_kind and gate_pg_id (64 hex digits) of the
# programs the server runs, checked.
gate_pg_identify() {
    if [ -n "$gate_pg_image" ] && [ -n "$gate_pg_bin" ]; then
        echo "gate: SPAWN_GATE_PG_IMAGE and SPAWN_GATE_PG_BIN are both set: the gate's server runs from one of them" >&2
        return 1
    fi
    if [ -n "$gate_pg_bin" ]; then
        case "$gate_pg_bin" in
        /?*) ;;
        *)
            echo "gate: SPAWN_GATE_PG_BIN=$gate_pg_bin is not an absolute path" >&2
            return 1
            ;;
        esac
        gate_pg_bin=${gate_pg_bin%/}
        if [ ! -f "$gate_pg_bin/postgres" ] || [ ! -x "$gate_pg_bin/postgres" ] ||
            [ ! -f "$gate_pg_bin/initdb" ] || [ ! -x "$gate_pg_bin/initdb" ]; then
            echo "gate: SPAWN_GATE_PG_BIN=$gate_pg_bin has no postgres and initdb" >&2
            return 1
        fi
        gate_pg_prefix=$(dirname "$gate_pg_bin")
        gate_pg_lang=${LANG:-C.UTF-8}
        gate_pg_kind=programs
        gate_pg_id=$({
            "$gate_pg_bin/postgres" --version &&
                sha256sum "$gate_pg_bin/postgres" "$gate_pg_bin/initdb" | cut -d' ' -f1 &&
                echo "LANG=$gate_pg_lang"
        } | sha256sum | cut -d' ' -f1) || gate_pg_id=""
    else
        command -v docker >/dev/null 2>&1 || {
            echo "gate: SPAWN_GATE_PG_IMAGE=$gate_pg_image needs docker to read the image" >&2
            return 1
        }
        gate_pg_kind=image
        gate_pg_id=$(timeout 60 docker image inspect --format '{{.Id}}' "$gate_pg_image" 2>/dev/null) || {
            echo "gate: SPAWN_GATE_PG_IMAGE=$gate_pg_image: docker has no such image (docker pull it)" >&2
            return 1
        }
        gate_pg_id=${gate_pg_id#sha256:}
    fi
    case "$gate_pg_id" in
    '' | *[!0-9a-f]*)
        echo "gate: no id for the programs of $gate_pg" >&2
        return 1
        ;;
    esac
}

# gate_pg_export <made dir>: the image's root in <made>/root and its PATH
# and LANG in <made>/env.
gate_pg_export() {
    mkdir "$1/root" || return 1
    container=$(timeout 60 docker create "sha256:$gate_pg_id") || return 1
    # Device nodes cannot be made without root, and bubblewrap gives the
    # server a /dev of its own.
    exported=0
    timeout 600 docker export -o "$1/root.tar" "$container" || exported=$?
    docker rm "$container" >/dev/null 2>&1
    if [ "$exported" -ne 0 ] || ! tar -x -C "$1/root" --exclude='dev/*' -f "$1/root.tar"; then
        echo "gate: could not export the image $gate_pg_image" >&2
        return 1
    fi
    rm -f "$1/root.tar"
    # The image's PATH and LANG (Debian's images keep postgres in
    # /usr/lib/postgresql/<major>/bin), as its entrypoint has them.
    docker image inspect --format '{{range .Config.Env}}{{println .}}{{end}}' \
        "sha256:$gate_pg_id" | grep -E '^(PATH|LANG)=' >"$1/env"
    grep -q '^PATH=' "$1/env" || echo 'PATH=/usr/local/bin:/usr/bin:/bin' >>"$1/env"
    grep -q '^LANG=' "$1/env" || echo 'LANG=C.UTF-8' >>"$1/env"
    found=""
    old_ifs=$IFS
    IFS=:
    for bin in $(sed -n 's/^PATH=//p' "$1/env"); do
        [ -x "$1/root$bin/postgres" ] && [ -x "$1/root$bin/initdb" ] && found=1
    done
    IFS=$old_ifs
    if [ -z "$found" ]; then
        echo "gate: the image $gate_pg_image has no postgres and initdb on its PATH" >&2
        return 1
    fi
}

# gate_pg_prepare: the programs' template (and an image's root), made once
# per id.
gate_pg_prepare() {
    gate_pg_identify || return 1
    gate_pg_cache=${XDG_CACHE_HOME:-$HOME/.cache}/spawn/gate-pg
    gate_pg_dir=$gate_pg_cache/$gate_pg_id
    gate_pg_root=-
    [ "$gate_pg_kind" != image ] || gate_pg_root=$gate_pg_dir/root
    if [ ! -f "$gate_pg_dir/ready" ]; then
        mkdir -p "$gate_pg_cache" && chmod 700 "$gate_pg_cache" || return 1
        (
            exec 6>>"$gate_pg_cache/$gate_pg_id.lock"
            if command -v flock >/dev/null 2>&1; then flock 6; fi
            [ ! -f "$gate_pg_dir/ready" ] || exit 0
            rm -rf "$gate_pg_dir"
            made=$(mktemp -d "$gate_pg_cache/.$gate_pg_id.XXXXXX") || exit 1
            trap 'chmod -R u+w "$made" 2>/dev/null; rm -rf "$made"' EXIT
            mkdir "$made/template" "$made/users" && chmod 700 "$made/template" || exit 1
            made_root=-
            if [ "$gate_pg_kind" = image ]; then
                gate_pg_export "$made" || exit 1
                made_root=$made/root
            else
                printf 'PATH=%s:/usr/local/bin:/usr/bin:/bin\nLANG=%s\n' "$gate_pg_bin" "$gate_pg_lang" >"$made/env"
            fi
            cp "$made/env" "$made/users/env"
            gate_pg_users "$made/users"
            (umask 077 && od -An -N24 -tx1 /dev/urandom | tr -d ' \n' >"$made/password") || exit 1
            cp "$made/password" "$made/users/password"
            gate_pg_bwrap "$made_root" "$made/users" "$made/template" - "$made/users/password" \
                initdb --username=postgres --auth-local=scram-sha-256 --auth-host=scram-sha-256 \
                --pwfile=/tmp/pwfile --no-sync </dev/null >"$made/initdb.log" 2>&1 || {
                echo "gate: initdb of $gate_pg failed:" >&2
                tail -n 20 "$made/initdb.log" >&2
                exit 1
            }
            echo 'CREATE DATABASE spawn_test' | gate_pg_bwrap "$made_root" "$made/users" "$made/template" - - \
                postgres --single -D /var/lib/postgresql/data postgres >>"$made/initdb.log" 2>&1 || {
                echo "gate: the template of $gate_pg has no spawn_test" >&2
                exit 1
            }
            touch "$made/ready"
            mv -T "$made" "$gate_pg_dir" || exit 1
            trap - EXIT
        ) || {
            echo "gate: could not prepare the private test server from $gate_pg in $gate_pg_cache" >&2
            return 1
        }
    fi
    # Used: spawn-maintenance removes a template no gate used for its age.
    touch "$gate_pg_dir/ready"
}

# gate_pg_keep <dir>: the template and its password, both of one making:
# the password is read again after the copy, and a template remade in
# between (a new random password) is copied again from the new one.
# Returns 1 when the template cannot be made (gate_pg_prepare: the
# programs), 2 when it cannot be copied (the host's: its disk, or the
# template remade under every copy), with the reason in gate_pg_said. The
# suite's seam, SPAWN_GATE_TEST_PG_KEEP_FAIL=1, makes every copy look
# remade.
gate_pg_keep() {
    gate_pg_said=""
    mkdir -m 700 "$1" || { gate_pg_said="cannot make $1"; return 2; }
    for _ in 1 2 3; do
        kept_password=$(cat "$gate_pg_dir/password" 2>/dev/null) &&
            cp -a --reflink=auto "$gate_pg_dir/template" "$1/template" 2>/dev/null &&
            [ "$(cat "$gate_pg_dir/password" 2>/dev/null)" = "$kept_password" ] &&
            [ -z "${SPAWN_GATE_TEST_PG_KEEP_FAIL-}" ] && {
            (umask 077 && printf '%s' "$kept_password" >"$1/password") ||
                { gate_pg_said="cannot write $1/password"; return 2; }
            return 0
        }
        chmod -R u+w "$1/template" 2>/dev/null
        rm -rf "$1/template"
        gate_pg_prepare || return 1
    done
    gate_pg_said="its template in $gate_pg_dir could not be copied whole: missing, or remade while it was copied, three times"
    echo "gate: the private test server's $gate_pg_said" >&2
    return 2
}

gate_pg_kept=""
gate_pg_start() { # <dir> <jobs> [<option>]
    gate_pg_prepare || return 1
    mkdir -m 700 "$1" "$1/socket" "$1/users" || return 2
    cp "$gate_pg_dir/env" "$1/users/env" || return 2
    gate_pg_users "$1/users"
    gate_pg_started=$(date +%s%3N)
    # The gate's own copy of the template and its password, taken at its
    # first start: every later start (the candidate phase's fresh server)
    # copies that, whatever became of the shared one, which another gate
    # remakes with a new password once it is gone (spawn-maintenance's,
    # or a hand's, removal), while the sandbox's URL keeps the first.
    if [ -z "$gate_pg_kept" ]; then
        gate_pg_keep "$1/kept" || {
            keep_rc=$?
            [ -z "$gate_pg_said" ] || printf '%s\n' "$gate_pg_said" >"$1/start.said"
            return "$keep_rc"
        }
        gate_pg_kept=$1/kept
    fi
    cp -a --reflink=auto "$gate_pg_kept/template" "$1/data" || return 2
    gate_pg_bwrap "$gate_pg_root" "$1/users" "$1/data" "$1/socket" - \
        postgres -c listen_addresses= -c unix_socket_directories=/run/postgresql \
        -c max_connections=$((100 + 50 * $2)) -c fsync=off -c synchronous_commit=off \
        -c full_page_writes=off -c max_wal_size=4GB ${3:+-c "$3"} </dev/null >"$1/server.log" 2>&1 &
    gate_pg_pid=$!
    gate_pg_socket=$1/socket/.s.PGSQL.5432
    waited=0
    until grep -q 'database system is ready to accept connections' "$1/server.log" 2>/dev/null; do
        if ! kill -0 "$gate_pg_pid" 2>/dev/null || [ "$waited" -ge 600 ]; then
            echo "gate: the private test server did not start:" >&2
            tail -n 20 "$1/server.log" >&2
            return 2
        fi
        sleep 0.05
        waited=$((waited + 1))
    done
    [ -S "$gate_pg_socket" ] && [ ! -h "$gate_pg_socket" ] || {
        echo "gate: the private test server made no socket at $gate_pg_socket" >&2
        return 2
    }
    gate_pg_password=$(cat "$gate_pg_kept/password") || return 2
    gate_pg_ms=$(($(date +%s%3N) - gate_pg_started))
}

# gate_pg_stop: run from the gate's EXIT trap, under `set -e`: nothing in it
# may fail (a `wait` on the killed server returns 137, which would end the
# gate with that status instead of its own).
gate_pg_stop() {
    [ -n "$gate_pg_pid" ] || return 0
    kill -KILL "$gate_pg_pid" 2>/dev/null || :
    wait "$gate_pg_pid" 2>/dev/null || :
    gate_pg_pid=""
}
