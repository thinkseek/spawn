# PostgreSQL URLs, read the way libpq reads them: one parser for every
# script that turns SPAWN_TEST_DATABASE_URL (or another postgres URL) into
# psql's connection parameters (the gate, scripts/e2e.sh, the checks and
# objective scenarios in rsi/scenarios). Source this file (POSIX sh).
#
#   postgres://user:password@host:port/database?parameters
#
# - Userinfo, host and database are percent-decoded: `p%40ss` is `p@ss`.
# - The host is a name, an IPv4 address, an IPv6 literal in brackets
#   (`[::1]`), or a percent-encoded socket directory (`%2Frun%2Fpostgresql`);
#   empty means libpq's default socket directory. A `host=` / `port=`
#   parameter overrides the authority's, as in libpq
#   (`postgres://u:p@/db?host=/run/postgresql`).
# - The port defaults to 5432.
#
# pg_url_parse <url> sets, or returns 1 for what is not a single-host
# postgres URL (a list of hosts, no `scheme://`):
#   pg_url_scheme    postgres or postgresql
#   pg_url_userinfo  `user:password` as written (still encoded), or empty
#   pg_url_user      decoded
#   pg_url_password  decoded; empty when the URL has none
#   pg_url_host      decoded: a name, an address (no brackets) or a directory
#   pg_url_port      digits
#   pg_url_database  decoded; empty when the URL names none
#   pg_url_options   the other parameters, as written, `&`-separated, no `?`
#
# pg_url_server <url> prints the URL of the server alone, host and port in
# the authority, encoded: scheme://userinfo@host:port. Appending
# `/<database>` names a database on it, whatever form the input took.
#
# pg_url_export <url> exports PGUSER, PGPASSWORD, PGHOST and PGPORT for
# psql (and unsets PGPASSWORD when the URL has none).

# pg_url_decode <text>: %XX escapes decoded, everything else as it is. A
# `%` not followed by two hex digits stays; %00 is dropped (no shell
# variable holds a NUL).
pg_url_decode() {
    case "$1" in
    *%*) ;;
    *)
        printf '%s' "$1"
        return 0
        ;;
    esac
    printf '%s' "$1" | LC_ALL=C awk '
        BEGIN { for (i = 0; i < 256; i++) hex[sprintf("%02x", i)] = i }
        {
            if (NR > 1) printf "\n"
            line = $0
            out = ""
            while ((at = index(line, "%")) > 0) {
                out = out substr(line, 1, at - 1)
                code = tolower(substr(line, at + 1, 2))
                if (code ~ /^[0-9a-f][0-9a-f]$/) {
                    if (hex[code] != 0) out = out sprintf("%c", hex[code])
                    line = substr(line, at + 3)
                } else {
                    out = out "%"
                    line = substr(line, at + 1)
                }
            }
            printf "%s", out line
        }'
}

# pg_url_encode_host <host>: the host as the authority spells it: a socket
# directory percent-encoded, an IPv6 address in brackets, a name as it is.
pg_url_encode_host() {
    case "$1" in
    /*) printf '%s' "$1" | sed -e 's/%/%25/g' -e 's|/|%2F|g' -e 's/:/%3A/g' -e 's/@/%40/g' \
        -e 's/?/%3F/g' -e 's/#/%23/g' -e 's/ /%20/g' -e 's/,/%2C/g' ;;
    *:*) printf '[%s]' "$1" ;;
    *) printf '%s' "$1" ;;
    esac
}

pg_url_parse() {
    pg_url_scheme=${1%%://*}
    case "$pg_url_scheme" in
    postgres | postgresql) ;;
    *) return 1 ;;
    esac
    pg_url_rest=${1#*://}
    pg_url_options=""
    case "$pg_url_rest" in
    *\?*)
        pg_url_query=${pg_url_rest#*\?}
        pg_url_rest=${pg_url_rest%%\?*}
        ;;
    *) pg_url_query="" ;;
    esac
    pg_url_authority=${pg_url_rest%%/*}
    case "$pg_url_rest" in
    */*) pg_url_database=$(pg_url_decode "${pg_url_rest#*/}") ;;
    *) pg_url_database="" ;;
    esac
    case "$pg_url_authority" in
    *@*)
        pg_url_userinfo=${pg_url_authority%@*}
        pg_url_hostport=${pg_url_authority##*@}
        ;;
    *)
        pg_url_userinfo=""
        pg_url_hostport=$pg_url_authority
        ;;
    esac
    case "$pg_url_userinfo" in
    *:*)
        pg_url_user=$(pg_url_decode "${pg_url_userinfo%%:*}")
        pg_url_password=$(pg_url_decode "${pg_url_userinfo#*:}")
        ;;
    *)
        pg_url_user=$(pg_url_decode "$pg_url_userinfo")
        pg_url_password=""
        ;;
    esac
    case "$pg_url_hostport" in
    *,*) return 1 ;;
    \[*\]:*)
        pg_url_host=${pg_url_hostport%%]*}
        pg_url_host=${pg_url_host#\[}
        pg_url_port=${pg_url_hostport##*]:}
        ;;
    \[*\])
        pg_url_host=${pg_url_hostport#\[}
        pg_url_host=${pg_url_host%]}
        pg_url_port=""
        ;;
    *:*)
        pg_url_host=$(pg_url_decode "${pg_url_hostport%:*}")
        pg_url_port=${pg_url_hostport##*:}
        ;;
    *)
        pg_url_host=$(pg_url_decode "$pg_url_hostport")
        pg_url_port=""
        ;;
    esac
    # Parameters: host, port, user, password and dbname override the URL's
    # parts; the rest are kept as written.
    pg_url_old_ifs=$IFS
    IFS='&'
    for pg_url_pair in $pg_url_query; do
        IFS=$pg_url_old_ifs
        pg_url_value=$(pg_url_decode "${pg_url_pair#*=}")
        case "$pg_url_pair" in
        host=*) pg_url_host=$pg_url_value ;;
        port=*) pg_url_port=$pg_url_value ;;
        user=*) pg_url_user=$pg_url_value ;;
        password=*) pg_url_password=$pg_url_value ;;
        dbname=*) pg_url_database=$pg_url_value ;;
        "") ;;
        *) pg_url_options=${pg_url_options:+$pg_url_options&}$pg_url_pair ;;
        esac
    done
    IFS=$pg_url_old_ifs
    case "$pg_url_host" in *,*) return 1 ;; esac
    case "${pg_url_port:-5432}" in
    *[!0-9]*) return 1 ;;
    esac
    pg_url_port=${pg_url_port:-5432}
    # A user or password moved to the parameters is moved into the userinfo,
    # so pg_url_server carries them.
    pg_url_userinfo=""
    if [ -n "$pg_url_user" ]; then
        pg_url_userinfo=$(pg_url_encode_component "$pg_url_user")
        [ -z "$pg_url_password" ] ||
            pg_url_userinfo="$pg_url_userinfo:$(pg_url_encode_component "$pg_url_password")"
    fi
    return 0
}

# pg_url_encode_component <text>: a user, password or database name with
# what a URL part cannot hold as it is percent-encoded.
pg_url_encode_component() {
    printf '%s' "$1" | sed -e 's/%/%25/g' -e 's/:/%3A/g' -e 's/@/%40/g' -e 's|/|%2F|g' \
        -e 's/?/%3F/g' -e 's/#/%23/g' -e 's/ /%20/g' -e 's/&/%26/g'
}

pg_url_server() {
    pg_url_parse "$1" || return 1
    printf '%s://%s%s:%s' "$pg_url_scheme" "${pg_url_userinfo:+$pg_url_userinfo@}" \
        "$(pg_url_encode_host "$pg_url_host")" "$pg_url_port"
}

pg_url_export() {
    pg_url_parse "$1" || return 1
    PGUSER=$pg_url_user
    PGHOST=$pg_url_host
    PGPORT=$pg_url_port
    export PGUSER PGHOST PGPORT
    if [ -n "$pg_url_password" ]; then
        PGPASSWORD=$pg_url_password
        export PGPASSWORD
    else
        unset PGPASSWORD
    fi
    # An empty user or host leaves libpq's defaults.
    [ -n "$PGUSER" ] || unset PGUSER
    [ -n "$PGHOST" ] || unset PGHOST
}
