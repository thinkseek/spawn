#!/usr/bin/env python3
"""The credential values a value-based scan looks for, read from the credential files.

The repository's copy (scripts/redact/secret-values.py, issue 98): deploy/deploy.sh installs it
beside the spawn binary and replaces the push scan's copy (/var/tmp/spawn) when it differs; the
root-owned copy the redaction and the store's file scan load (/usr/local/lib/spawn/redact) is
installed by hand, and a deploy says when it differs. Vendored from that redact copy (sha256
85bd2d12, 2026-11-17), the newer of the two: the push scan's copy lacked locate(). Stdlib only.

One list for every scan (the git scan of a push, the scan of files before a store upload), so
they cannot drift apart. Nothing here prints a value.

A module only (cc-astra A51): there is no command, and no entry point here writes or prints a
value. A scan loads this file and matches in its own process (secret-scan-added.py for a git
range, secret-scan-files.py for files). A caller refuses when a source is missing (and not on
ACCEPTED_MISSING) or cannot be read: a scan with part of the list must not be able to say clean.
               read(home, root="/") -> (values, missing, unreadable, accepted)
               read_named(home, root="/") -> the same, with values a dict: value -> the names of where it was read
               (source file and key, never a value), for a tool that says WHICH credential a scan found.
               A key is part of a name only when it cannot be a secret itself (rev-g, review-scan-next.md
               F1, F2): it has the shape of a variable name, no part of it looks like a token, and it
               holds none of the values. Otherwise the name is the source file alone.
               read_dead(home, root="/") -> the values that are ONLY retired and whose retired line is
               marked dead (see RETIRED): a push scan reports them and does not refuse for them.
               locate(home, root="/") -> where each value lies, never a value (objective 175, for the
               transcript archive's mask): {"sources": [{path, size, mtime_ns, dev, ino, ctime_ns}], "entries": [...],
               "missing", "unreadable", "accepted"}. An entry names a source path, a line number (the
               file split at "\n" and NUL, as read() splits it) or a JSON pointer of sorted-key and list
               indexes (never a key), a byte span, a transform and the value's printable name (as
               read_named() names it, shortened to the file's base name and key). Transforms: "raw"
               (the bytes at the span), "pct" (the span percent-decoded as urllib.parse.unquote does,
               then the bytes at "sub" of that), "short" (a short URL password's shape, built from the
               "user" and "pw" places by its "form": "pair" is pre + user + ":" + pw + post, "b64"
               is base64 of user + ":" + pw with no "=" padding, "word" is pre + pw + post; the
               pre and post texts are the collector's literal shape text, never a value). A value whose place cannot be
               found makes locate() raise: a list it cannot place in full is no list; so does a place
               that does not hold its value when read back from the raw bytes as the Bridge's reader
               reads them (lines split at \r\n, a lone \r, \n and NUL, as read() splits them: rev-g
               F1). "accepted_paths": each accepted-missing source's absolute path; "watch": each
               source's own directory (where an older copy appears), each settings glob's literal
               base (a new workspace) and existing .claude directories, with its mtime_ns (null when
               it does not exist); "probe": `<x>/.claude` for each existing workspace `<x>` under a
               glob's base, with whether it exists (never `<x>` itself, whose files change all day).
               The reader runs locate() again when a watched mtime or a probed existence changes
               (the lead's R3 ruling, rev-g N1). One generation (R4, cc-astra A75 and A77): the watch
               and probe are stamped before the collection, and after it they and every source (by
               the fd its places are read back from, and by its path at the end) are checked again; on a change it collects
               again, at most LOCATE_TRIES times, then raises Unsettled. Lengths are
               revealed by spans (accepted, lead 11-14); no value, hash of one or key is.
"""
import base64
import glob
import json
import os
import re
import sys
import urllib.parse

# locate()'s recorder (objective 175): None outside locate(). While it runs, each value added is
# noted with its place in the text being read (_CTX), or in the percent-decoded part it came from
# (_DERIVE). Nothing here changes what read(), read_named() or read_dead() return.
_REC = None
_CTX = None
_DERIVE = None
_STAT = None
_TRIED = None
# A test's barrier (objective 175 R4, cc-astra A75/A77): called with a point's name ("enumerated":
# the sources are read; "checked": their places are read back) so that a test can change a source
# or a watched directory there. None in every real run.
_BARRIER = None
# How many times locate() collects again when a source or a watched directory changed while it
# collected, before it refuses (Unsettled).
LOCATE_TRIES = 3

# Home-relative sources of KEY=VALUE lines, of whole-line values, and of JSON string leaves.
KV = [".local/share/spawn-rsi/spawn.env", ".config/spawn/r2.env", ".config/spawn/typesafe.env",
      ".hermes/.env", ".config/spawn/model-relay.env", ".config/spawn/r2-store.env"]
# A process environment written to a file (NUL between its entries): PATH, HOME and LANG beside a
# database URL. Read like the other sources that mix settings with credentials: credentials only.
# (As a KEY=VALUE source it was one long "line" under its first entry's name, and its URL's
# password carried that name: SPAWN_INTEGRATE_PID.)
MIXED = [".config/spawn/gate-env"]
# Values that were rotated out (the owner's rule, lead-mac 2026-11-13 10:01): before a secret file
# changes, its old value is appended here as RETIRED_<date>_<NAME>=<value>, by pipe. They stay in
# the list for good: reported by every scan and replaced by the redaction like any other value.
# A push scan refuses for a retired value too, until its credential is known to be dead at its
# issuer; that is said here by a line DEAD_RETIRED_<date>_<NAME>=<anything> (never read as a
# value). read_dead() gives the values that are only retired and marked dead. The file may be
# absent (a host where nothing was rotated); its absence is named in every scan's output.
# A DEAD line names its retired line by name, so it counts only while that name holds ONE value:
# a name written twice with two values is dead for neither (rev-g N12). Copies of the file made
# beside it (retired-values.env.bak, ...) are read as retired values too; DEAD lines count from
# the file itself only (rev-g N13).
RETIRED = ".config/spawn/retired-values.env"
RETIRED_NOTE = " (retired)"
DEAD_PREFIX = "DEAD_"
WHOLE = [".config/spawn/test-database.url", ".config/spawn/backup.key",
         ".config/spawn/fleet-test-database.url", ".config/spawn/fleet-test-admin.pw"]
JSON_LEAVES = [".claude/.credentials.json"]
# Credentials Keep delivers (~/.keep on the owner's Mac, /run/keep here), Keep first: Keep id -> the
# home-relative plaintext file it replaced, or None for one that never had one
# (scripts/keep/credentials.manifest names the same pairs as its id and legacy_file, "-" for None;
# scripts/redact/test-secret-values.sh checks they agree). /run/keep/<id> is
# read whole, as a WHOLE_ABSOLUTE source; the plaintext file, while it exists, is read as before (a
# KV source). Keep's file may be absent while the plaintext file gave at least one value, and the
# plaintext file while Keep's file is not empty: it is then missing by acceptance (named in the
# output, see MOVED_TO_KEEP). Both absent, or one absent and the other empty (for the plaintext
# file: no value, as when it holds only comments or KEY=), is a refusal: a scan with part of the
# list must not say clean.
KEEP_DIR = "/run/keep"
KEEP = {"typesafe.api-key": ".config/spawn/typesafe.env"}
# Absolute sources (the reviewers' test database, then Keep's files).
WHOLE_ABSOLUTE = ["/data/tmp/ccd-pg/url", "/data/tmp/ccd-pg/pw"] + [KEEP_DIR + "/" + k for k in KEEP]
# Sources that may be absent: the earlier host's file, and the reviewers' test database, which is
# temporary (its credentials go when it goes). Each is named in the output when absent.
ACCEPTED_MISSING = {".hermes/.env", "/data/tmp/ccd-pg/url", "/data/tmp/ccd-pg/pw", RETIRED}
# A source that was renamed on purpose: absent by acceptance only while the file it became exists
# (the backup key under two names, design-backup-two-keys.md: backup.key becomes backup.key.k1 by
# mv, and backup.key.k2 is made beside it). Files named after a source are read whole as its older
# copies whatever their name, so backup.key.k1 and backup.key.k2 are read as soon as they exist;
# what this adds is that the old name's absence stops being a refusal once .k1 is there.
RENAMED = {".config/spawn/backup.key": ".config/spawn/backup.key.k1"}
# A source retired into Keep (owner t475u, 2026-11-17 12:01: typesafe.env deleted, its value delivered to
# /run/keep by Keep; generalized to every KEEP entry, issue 98): the plaintext file is absent by
# acceptance only while its Keep file exists and is not empty, and that file is read as a WHOLE_ABSOLUTE
# source, so the list loses no value. The other way round, the fallback: the Keep file is absent by
# acceptance only while the plaintext file gave at least one value (before Keep delivers, or a host
# without Keep). A plaintext file that is there but gives none (empty, comments only, KEY=) is no
# fallback: Keep's file refuses. If both are gone, both refuse.
MOVED_TO_KEEP = {rel: KEEP_DIR + "/" + k for k, rel in KEEP.items() if rel}

# Settings that are not secrets (reviewed): a private LAN range, the tmux socket name, local listen
# and relay addresses, the judge model name.
NOT_SECRET = {"SPAWN_SERVE_OPEN_NETWORKS", "SPAWN_TMUX_SOCKET", "SPAWN_SERVE_ADDRESS",
              "SPAWN_JUDGE_MODEL", "SPAWN_JUDGE_URL", "SPAWN_MODEL_RELAY"}
# Bucket names are not secrets (they are literals in docs on main and in the queue); a value under
# one of these keys is skipped only while every element has a bucket name's shape.
BUCKET_KEYS = {"SPAWN_R2_BUCKET", "R2_BUCKETS"}
BUCKET = re.compile(r"[a-z0-9][a-z0-9-]{1,61}[a-z0-9]")
# A value that starts with "/" is left out only when it is a path: its key says so, or it names
# something that exists. (Dropping every such value would lose a base64 secret one time in 64.)
PATH_KEY = re.compile(r"(_DIR|_PATH|_FILE|_ROOT|_BIN|_LOG|DIR)$")
# String leaves of a credentials JSON that are not credentials (the store's first scan, 2026-11-13:
# a plugin's server name and its public URLs were counted as values and named six files). A leaf
# under one of these keys is skipped only while it has the shape its key says: a URL with no
# user part, query or fragment (those can carry a token), or a short plain name; and, for both,
# while no part of it looks like a token (rev-g F3: a capability URL carries its secret in a path
# segment or a host label).
NOT_SECRET_LEAF_URL = {"serverUrl", "authorizationServerUrl", "authServerMetadataUrl", "resourceMetadataUrl"}
NOT_SECRET_LEAF_NAME = {"serverName"}
PLAIN_URL = re.compile(r"https?://[^\s@?#]+")
PLAIN_NAME = re.compile(r"[A-Za-z0-9_.:-]{1,64}")
# A key that may be printed as part of a name: a variable's shape, nothing else.
KEY_NAME = re.compile(r"[A-Za-z_][A-Za-z0-9_]{0,63}")
URL_PASSWORD = re.compile(r"://([^:/@\s]+):([^@\s]+)@")
PASSWORD_NOTE = " (the password inside its URL)"
# A URL password under 6 characters cannot be looked for alone (it is a common word's length),
# so it is looked for as it stands in any URL: "user:password@". Found 2026-11-13 (cc-sol): the
# live database's password was a 5-letter default that no list carried.
# Outside a URL the same short password stands in four more shapes, each with context of its
# own, so only the exact password behind that context matches (rev-g F9, F10):
# - the word PASSWORD, Password or password followed by the password in each spelling of
#   SHORT_PASSWORD_AROUND: =pw, ='pw', ="pw" (a shell or env line; PASSWORD=pw is also the end
#   of PGPASSWORD=, POSTGRES_PASSWORD=, DB_PASSWORD=, and password=pw is libpq's keyword),
#   ": pw" (YAML), and "…": "pw" with and without the space (JSON);
# - a .pgpass line's end :<user>:<pw>;
# - HTTP Basic, the base64 of <user>:<pw> (as base64 only: plain, it is the URL shape without
#   its @ and would name "user:passwordlonger").
# The unquoted shapes match a longer word that starts with the password too (password=<pw>xyz):
# a false name now and then, never a miss.
# NO FORM EXISTS for a short password alone (typed at a prompt, a file holding only it): it is
# a common word's length. Rotation to a long random value is its answer; whatever this list
# misses also stays unredacted.
SHORT_PASSWORD_AROUND = [("=", ""), ("='", "'"), ('="', '"'), (": ", ""), ('": "', '"'), ('":"', '"')]
SHORT_PASSWORD_NOTE = " (a short password inside its URL, looked for in its contexts: user:password@ and others)"

# Two kinds of source hold settings beside credentials, so only a credential is read from them
# (their other values, old relative paths, model names, switches like "1", would be found in
# every file): an entry whose NAME is a credential's, the password inside a URL, and any part of
# any value that has a token's shape (rev-g, review-scan-next.md F1: headers such as
# ANTHROPIC_CUSTOM_HEADERS, a DSN's user part, a key in a query or a path carry a credential
# under a name that says nothing).
# 1. Older copies of a source, kept beside it under a longer name (spawn.env.bak-relay,
#    r2.env.old, test-database.url~): found by name at run time, for every KV and WHOLE source
#    (rev-g N3). They can hold a value that is no longer in the file itself. A copy of a WHOLE
#    source is read whole, like the source.
# 2. The "env" object of Claude Code settings files: it is given to every tool shell of every
#    session that reads the file, so "env" in a shell prints it. HOME's own files, and the
#    project-level ones in the places sessions run here (rev-g N4). All may be absent. A settings
#    file that is not valid JSON, even for the moment of an edit, is "unreadable": every scan
#    then refuses (exit 2) until it parses again (rev-g N2); that is meant.
#    The /data/tmp/ccd/*/.claude files are the reviewers' own worktrees: one of them left
#    malformed by a peer refuses every scan on the host (owner pushes included) until it is
#    mended. That fails closed and is meant (rev-g N6); the refusal names the file.
SETTINGS_ENV = [".claude/settings.json", ".claude/settings.local.json"]
SETTINGS_ENV_GLOBS = ["projects/.claude/settings*.json", "projects/*/.claude/settings*.json",
                      ".local/share/spawn-rsi/ws/*/.claude/settings*.json"]
SETTINGS_ENV_GLOBS_ABSOLUTE = ["/data/tmp/ccd/*/.claude/settings*.json"]
ACCEPTED_MISSING |= set(SETTINGS_ENV)
# A credential's name: one of these as a whole word of the name (words are split at "_", "-", "."
# and at a lower-to-upper change), or a word ending in one of the endings. Whole words, because
# AUTH is inside GIT_AUTHOR_EMAIL, PASS inside BYPASS, KEY inside KEYBOARD, and TOKENS is a count
# (rev-g F2): an email address as a "credential" would name every file.
CREDENTIAL_WORDS = {"TOKEN", "KEY", "SECRET", "PASSWORD", "PASSWD", "PASS", "PWD", "AUTH",
                    "CREDENTIAL", "CREDENTIALS", "COOKIE", "APIKEY", "BEARER", "JWT", "SIGNATURE",
                    "PRIVATE", "SESSION", "DSN"}
CREDENTIAL_ENDINGS = ("PASSWORD", "PASSWD", "APIKEY", "SECRET", "TOKEN")
NAME_WORDS = re.compile(r"[_.\-]+|(?<=[a-z0-9])(?=[A-Z])")
# What separates the parts of a value that are looked at for a token's shape.
VALUE_PARTS = r"[\s=:,;?&/@\"']+"
# A model's name is not a token: an entry whose name's last word is MODEL is skipped while its
# value is a short plain name with no token-shaped part between its "-", "_", ".", ":".
MODEL_NAME = re.compile(r"[A-Za-z0-9_.:\[\]-]{1,64}")
# Token-shaped parts that are not credentials, left out so that they do not name half the
# files of a tree (rev-g N5):
# - a mail address standing by itself under a name that says so (GIT_AUTHOR_EMAIL);
# - the host of a URL when none of its labels has a token's shape (a server name with digits),
#   or a percent-encoded socket directory in the host's place;
# - a file's name (a part ending in one of the listed extensions: a certificate bundle with a
#   date in its name);
# - the whole value of an entry whose name ends in PATH, DIR, FILE, HOME or CERTS when the
#   value is a place on disk (a tool's versioned directory, a commit in an old release path).
# NOT left out, because their shape is a token's: a UUID and a bare commit hash under any other
# name. If one is ever set here it becomes a "value" and its files become hits; the answer is
# then a reviewed exclusion by entry name, never a wider shape rule.
# A credential-named entry is never left out by these, whatever its value looks like.
URL_HOST = re.compile(r"://(?:[^/@\s?#]*@)?([^/:?#\s]+)")
FILE_NAME = re.compile(r".+\.(?:pem|crt|cer|json|jsonl|js|ts|py|sh|txt|log|toml|yaml|yml|conf|sock|so|tar|gz|zip|md)")
PATH_WORDS = {"PATH", "DIR", "FILE", "HOME", "CERTS"}
# A mail address standing by itself (not the user part of a URL, which follows "//" or ":") is
# not looked at for parts, under a name that says it is an address (a word EMAIL, MAIL, AUTHOR
# or COMMITTER): a long local part with a digit in it has a token's shape. Under any other name
# the local part is looked at: inbound-mail hooks use a token as an address (rev-g N7).
MAIL_WORDS = {"EMAIL", "MAIL", "AUTHOR", "COMMITTER"}
EMAIL = re.compile(r"(?<![:/\w.+-])[\w.+-]+@[\w-]+(?:\.[\w-]+)+(?![\w/-])")


def _credential_name(name):
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    return any(w in CREDENTIAL_WORDS or w.endswith(CREDENTIAL_ENDINGS) for w in words)


# Names that hold a credential though none of their words is a credential word: a URL with its
# password inside (SPAWN_DATABASE_URL, SPAWN_TEST_DATABASE_URL, FLEET_TEST_DATABASE_URL).
CREDENTIAL_NAME_ENDINGS = (("DATABASE", "URL"),)


def credential_name(name):
    """Whether a variable's name is a credential's name, for readers outside the collector (the
    174 guard). It is _credential_name plus CREDENTIAL_NAME_ENDINGS. The collector itself keeps
    _credential_name, so its outputs stay byte-identical: a ...DATABASE_URL entry of a mixed source
    already gives its whole URL (the URL rule) and its password (_add calls _add_passwords), and
    with the ending in _credential_name the same values and names come out in another order."""
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    return _credential_name(name) or any(tuple(words[-len(e):]) == e for e in CREDENTIAL_NAME_ENDINGS)


def _is_model_name(name, value):
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    return (bool(words) and words[-1] == "MODEL" and bool(MODEL_NAME.fullmatch(value))
            and not _token_shaped(value, r"[_.:\[\]-]"))


def _add_credentials_only(vals, name, value, source):
    """From a source that mixes settings with credentials: the value of a credential-named entry,
    the password inside a URL (and that URL), and every token-shaped part of any value. A
    credential-named entry gives its whole value AND its parts ("Bearer <t>" is seldom what a
    file holds; the bare <t> is: rev-g F6)."""
    global _DERIVE
    bare = _kv_value(value)
    if _credential_name(name):
        _add(vals, bare, name, source, name)
    elif _is_model_name(name, bare) or _is_path_setting(name, bare):
        return
    else:
        _add_passwords(vals, bare, source, name)
        if URL_PASSWORD.search(bare):
            _add(vals, bare, name, source, name)
    hosts = {m.group(1) for m in URL_HOST.finditer(bare)}
    for part in re.split(VALUE_PARTS, EMAIL.sub(" ", bare) if _mail_name(name) else bare):
        if not _token_shaped(part, r"$^") or FILE_NAME.fullmatch(part):
            continue
        if part in hosts and _plain_host(part):
            continue
        _add(vals, part, None, source, name)
        plain = urllib.parse.unquote(part)
        if plain != part:
            _DERIVE = part
            try:
                _add(vals, plain, None, source, name)
            finally:
                _DERIVE = None


def _mail_name(name):
    return any(w.upper() in MAIL_WORDS for w in NAME_WORDS.split(name) if w)


def _plain_host(host):
    """True for a URL's host that carries no token: a server name or address none of whose
    labels has a token's shape, or a socket directory written percent-encoded none of whose
    segments has. Asked only of a URL's host (rev-g F7: anywhere else a percent-encoded part,
    a signature in a query, is a credential)."""
    plain = urllib.parse.unquote(host)
    if plain != host:
        return plain.startswith("/") and not _token_shaped(plain, r"/")
    return not _token_shaped(host, r"\.")


def _is_path_setting(name, value):
    """True for an entry whose name's last word says it holds a place on disk and whose value is
    one (or a ":" list of them): no URL in it, every piece starting at "/", "~", "." or "$"."""
    words = [w.upper() for w in NAME_WORDS.split(name) if w]
    if not words or words[-1] not in PATH_WORDS or "://" in value:
        return False
    pieces = [p for p in value.split(":") if p]
    return bool(pieces) and all(p[0] in "/~.$" for p in pieces)


def _older_copies(home, rel):
    """Home-relative names of the files beside a source whose name starts with the source's."""
    folder, base = os.path.split(rel)
    try:
        names = os.listdir(os.path.join(home, folder))
    except OSError:
        return []
    return sorted(os.path.join(folder, n) for n in names
                  if n.startswith(base) and n != base and os.path.isfile(os.path.join(home, folder, n)))


def _token_shaped(text, separators):
    """True when a part of the text could be a token: 16 or more characters (the least a JSON leaf
    needs to count as a value) with a digit among them."""
    return any(len(part) >= 16 and any(c.isdigit() for c in part) for part in re.split(separators, text))


def _is_path(v, key):
    return v.startswith("/") and (bool(key and PATH_KEY.search(key)) or os.path.lexists(v))


def _kv_value(value):
    """The value of a KEY=VALUE line as a shell would read it: quotes off, a trailing # note off."""
    v = value.strip()
    if v[:1] in ("'", '"'):
        close = v.find(v[0], 1)
        if close > 0:
            return v[1:close]
    return re.split(r"\s+#", v, maxsplit=1)[0]


def _add(vals, v, key=None, source="", shown_key=""):
    """vals: value -> where it was read, as (source file, key, is the password part). The key is
    printed later only if _names() finds it cannot be a secret."""
    v = v.strip().strip('"').strip("'")
    if _is_path(v, key):
        return
    if len(v) >= 8:
        vals.setdefault(v, set()).add((source, shown_key, False))
        _note(v, _place(v))
    _add_passwords(vals, v, source, shown_key)


def _add_passwords(vals, text, source, shown_key):
    for m in URL_PASSWORD.finditer(text):
        if len(m.group(2)) >= 6:
            vals.setdefault(m.group(2), set()).add((source, shown_key, True))
            _note(m.group(2), _place(m.group(2)))
        elif len(m.group(1)) + len(m.group(2)) >= 6:
            user, short = m.group(1), m.group(2)
            pair = (user + ":" + short).encode("utf-8", "surrogateescape")
            shapes = [user + ":" + short + "@", ":" + user + ":" + short,
                      base64.b64encode(pair).decode("ascii").rstrip("=")]
            # Each shape's form for locate(): the literal text around the places, never a value.
            forms = [{"form": "pair", "pre": "", "post": "@"}, {"form": "pair", "pre": ":", "post": ""},
                     {"form": "b64"}]
            for word in ("PASSWORD", "Password", "password"):
                shapes += [word + before + short + after for before, after in SHORT_PASSWORD_AROUND]
                forms += [{"form": "word", "pre": word + before, "post": after}
                          for before, after in SHORT_PASSWORD_AROUND]
            for k, shape in enumerate(shapes):
                vals.setdefault(shape, set()).add((source, shown_key, SHORT_PASSWORD_NOTE))
                if _REC is not None:
                    _note(shape, dict(forms[k], t="short", k=k, user=_place(user), pw=_place(short)))


def _names(vals):
    """value -> printable names. A key is shown only when it has a variable's shape, no part of it
    looks like a token, and it holds none of the values; else the source file stands alone."""
    def shown(source, key, password):
        ok = (bool(key) and bool(KEY_NAME.fullmatch(key)) and not _token_shaped(key, r"_")
              and not any(v in key for v in vals))
        note = PASSWORD_NOTE if password is True else (password or "")
        return source + (":" + key if ok else "") + note
    return {v: {shown(*where) for where in wheres} for v, wheres in vals.items()}


def _not_secret_leaf(key, node):
    if key in NOT_SECRET_LEAF_URL:
        if not PLAIN_URL.fullmatch(node):
            return False
        host, _, path = node.split("://", 1)[1].partition("/")
        return not any(_token_shaped(part, r"$^") for part in host.split(".") + path.split("/"))
    if key in NOT_SECRET_LEAF_NAME:
        return bool(PLAIN_NAME.fullmatch(node)) and not _token_shaped(node, r"[_.:-]")
    return False


def _leaves(node, vals, source, key="", ptr=(), path=""):
    if isinstance(node, dict):
        order = sorted(node)
        for name, child in node.items():
            _leaves(child, vals, source, name if isinstance(name, str) else "", ptr + (order.index(name),), path)
    elif isinstance(node, list):
        for index, child in enumerate(node):
            _leaves(child, vals, source, key, ptr + (index,), path)
    elif isinstance(node, str) and len(node) >= 16 and not _not_secret_leaf(key, node):
        # The leaf's own key names it (if _names() lets it); the keys above it are never printed.
        _context(path, None, node, ptr)
        _add(vals, node, None, source, key)


class Unplaced(Exception):
    """locate() found a value whose place it cannot name: the list is not given (no value said)."""


class Unsettled(Unplaced):
    """locate() saw a source or a watched directory change in each of its LOCATE_TRIES collections:
    the list is not given."""


def _barrier(point):
    if _BARRIER is not None:
        _BARRIER(point)


def _stamp(st):
    """What tells a file read from another (R4, cc-astra A77): its size and mtime, and its device,
    inode and ctime, which userspace cannot set back: a same-length rewrite whose mtime is restored
    still moves the ctime, and a file renamed into place has another inode."""
    return (st.st_size, st.st_mtime_ns, st.st_dev, st.st_ino, st.st_ctime_ns)


def _context(path, number, text, ptr=None):
    """The text being read now, for locate(): a line (its number) or a JSON string (its pointer)."""
    global _CTX
    if _REC is not None:
        _CTX = {"path": path, "line": number, "ptr": list(ptr) if ptr is not None else None, "text": text}


def _span(text, start, end):
    """Byte offsets of text[start:end] in text as the file holds it (surrogateescape)."""
    try:
        return [len(text[:start].encode("utf-8", "surrogateescape")),
                len(text[:end].encode("utf-8", "surrogateescape"))]
    except UnicodeEncodeError:
        raise Unplaced("a value in text that is not bytes of the file") from None


def _place(x):
    """Where x lies in the current text, or in the percent-decoded part it came from."""
    if _REC is None:
        return None
    text = _CTX["text"] if _CTX else None
    if text is None:
        raise Unplaced("a value read outside any text")
    at = text.find(x)
    if at >= 0:
        return {"t": "raw", "span": _span(text, at, at + len(x))}
    if _DERIVE is not None:
        part_at = text.find(_DERIVE)
        plain = urllib.parse.unquote(_DERIVE)
        sub_at = plain.find(x)
        if part_at >= 0 and sub_at >= 0:
            return {"t": "pct", "span": _span(text, part_at, part_at + len(_DERIVE)),
                    "sub": _span(plain, sub_at, sub_at + len(x))}
    raise Unplaced("a value whose place in its source was not found")


def _note(value, place):
    if _REC is not None:
        where = {"path": _CTX["path"], "line": _CTX["line"], "ptr": _CTX["ptr"]}
        where.update(place)
        _REC.append((value, where))


def _short_name(shown):
    """A read_named() name cut to the source's base name and key, with no note: r2.env:R2_KEY."""
    source, _, key = shown.split(" (", 1)[0].partition(":")
    return os.path.basename(source) + (":" + key if key else "")


def _reader_lines(data):
    """The file split as read() splits it, from its raw bytes: universal newlines (\\r\\n, a lone
    \\r, \\n) and NUL. The Bridge's reader splits the same way."""
    return re.split(rb"\r\n|\r|\n|\0", data)


def _held(where, cache, stats):
    """The bytes a place names, read from the raw file as the Bridge's reader reads it. The file is
    the one collected (R4, A77): its fd's stamp, before and after the read, is the collection's."""
    path = where["path"]
    if path not in cache:
        with open(path, "rb") as f:
            before = _stamp(os.fstat(f.fileno()))
            data = f.read()
            if before != stats.get(path) or _stamp(os.fstat(f.fileno())) != before:
                raise Unsettled("a source that is not the file collected")
            cache[path] = data
    data = cache[path]
    if where["ptr"] is None:
        base = _reader_lines(data)[where["line"]]
    else:
        node = json.loads(data.replace(b"\0", b"\n").decode("utf-8", "surrogateescape"))
        for index in where["ptr"]:
            node = node[sorted(node)[index]] if isinstance(node, dict) else node[index]
        if not isinstance(node, str):
            raise Unplaced("a pointer that does not name a string")
        base = node.encode("utf-8", "surrogateescape")

    def at(place):
        start, end = place["span"]
        if end > len(base):
            raise Unplaced("a span past its text")
        part = base[start:end]
        if place["t"] == "raw":
            return part
        plain = urllib.parse.unquote(part.decode("utf-8", "surrogateescape")).encode("utf-8", "surrogateescape")
        start, end = place["sub"]
        return plain[start:end]

    if where["t"] != "short":
        return at(where)
    user, pw = at(where["user"]), at(where["pw"])
    if where["form"] == "pair":
        return where["pre"].encode() + user + b":" + pw + where["post"].encode()
    if where["form"] == "b64":
        return base64.b64encode(user + b":" + pw).rstrip(b"=")
    return where["pre"].encode() + pw + where["post"].encode()


def _check_places(recorded, stats):
    """Every place, read back from the raw bytes as the reader reads them, holds its value: else
    Unplaced (rev-g F1: a reader that reads other bytes there would mask the wrong ones). Each file
    read back is the one collected (R4): else Unsettled."""
    cache = {}
    for value, where in recorded:
        try:
            held = _held(where, cache, stats)
        except Unplaced:
            raise
        except OSError:
            raise Unsettled("a source gone or unreadable since it was collected") from None
        except (LookupError, ValueError, TypeError):
            raise Unplaced("a place the reader cannot read") from None
        if held != value.encode("utf-8", "surrogateescape"):
            raise Unplaced("a place that does not hold its value as the reader reads it")


def _glob_watch(pattern):
    """What tells the reader a file matching a settings glob may have appeared, cheaply (the
    lead's R3 ruling): (watched directories, probed paths).
    - Watched (by mtime): the glob's literal base (a new workspace moves it) and each existing
      directory the files sit in (its `.claude`: a new settings file moves it).
    - Probed (by existence only): `<x>/.claude` for each existing `<x>` matched under the base,
      never `<x>` itself, whose files change all day: a `.claude` added inside an existing
      workspace appears.
    A glob with no wildcard in its directory watches that directory, or probes it when absent."""
    folder = os.path.dirname(pattern)
    parent, last = os.path.split(folder)
    if glob.has_magic(last):
        raise ValueError("a settings glob whose last directory part is a pattern")
    if not glob.has_magic(parent):
        return ([folder], []) if os.path.isdir(folder) else ([], [folder])
    parts = parent.split("/")
    if sum(1 for part in parts if glob.has_magic(part)) > 1:
        # One wildcard level is what the probe covers; a deeper glob would miss a new middle level.
        raise ValueError("a settings glob with more than one wildcard directory level")
    first = next(i for i, part in enumerate(parts) if glob.has_magic(part))
    base = "/".join(parts[:first]) or "/"
    workspaces = sorted(d for d in glob.glob(parent) if os.path.isdir(d))
    probed = [os.path.join(d, last) for d in workspaces]
    return [base] + [p for p in probed if os.path.isdir(p)], probed


def _watch_set(home, root, tried):
    """The directories watched by mtime and the paths probed by existence (see _glob_watch), for
    the sources tried: each source's directory, and the settings globs'."""
    directories = {os.path.dirname(path) for path in tried}
    probed = set()
    patterns = ([os.path.join(home, p) for p in SETTINGS_ENV_GLOBS]
                + [os.path.join(root, p.lstrip("/")) for p in SETTINGS_ENV_GLOBS_ABSOLUTE])
    for pattern in patterns:
        watched, probes = _glob_watch(pattern)
        directories.update(watched)
        probed.update(probes)
    return sorted(directories), sorted(probed)


def _watch_stamps(directories, probed):
    """Each watched directory's mtime_ns (None: absent) and each probed path's existence, now."""
    watch = []
    for directory in directories:
        try:
            mtime = os.stat(directory).st_mtime_ns
        except OSError:
            mtime = None
        watch.append({"path": directory, "mtime_ns": mtime})
    return watch, [{"path": path, "exists": os.path.isdir(path)} for path in probed]


def _sources_hold(stats):
    """Every source read is, by its path, still the file collected (R4, A77)."""
    for path, stamp in stats.items():
        try:
            if _stamp(os.stat(path)) != stamp:
                return False
        except OSError:
            return False
    return True


def _collect_placed(home, root):
    """One collection, recorded: (vals, missing, unreadable, accepted, recorded, stats, tried)."""
    global _REC, _CTX, _STAT, _DERIVE, _TRIED
    _REC, _CTX, _STAT, _DERIVE, _TRIED = [], None, {}, None, set()
    try:
        vals, missing, unreadable, accepted, _ = _collect(home, root)
        return vals, missing, unreadable, accepted, _REC, _STAT, _TRIED
    finally:
        _REC, _CTX, _STAT, _DERIVE, _TRIED = None, None, None, None, None


def locate(home, root="/"):
    """Where each value lies (see the header): paths, line numbers, pointers, spans, transforms,
    stat stamps and names, never a value. Raises Unplaced when a value's place is not found, or a
    place does not hold its value when read back from the raw bytes as the reader reads them.

    One generation (R4, cc-astra A75 and A77): the watched directories and probed paths are stamped
    BEFORE the sources are collected, and after the collection and the places' check the watch set
    is derived again and stamped again, and every source is stat'd again by path. Any difference
    (a file created, rewritten or renamed while it ran, a directory moved) and it collects again,
    at most LOCATE_TRIES times, then raises Unsettled. So a file that appears after the baseline is
    either read, or moves a watched mtime or a probe against the baseline the reader compares with.
    A first run collects twice: its watch set is known only after a collection."""
    tried_before = set()
    for _ in range(LOCATE_TRIES):
        directories, probed = _watch_set(home, root, tried_before)
        watch, probe = _watch_stamps(directories, probed)
        try:
            vals, missing, unreadable, accepted, recorded, stats, tried = _collect_placed(home, root)
            _barrier("enumerated")
            if set(vals) - {v for v, _ in recorded}:
                raise Unplaced("a value that was not placed")
            _check_places(recorded, stats)
        except Unsettled:
            continue
        _barrier("checked")
        settled = ((directories, probed) == _watch_set(home, root, tried)
                   and (watch, probe) == _watch_stamps(directories, probed)
                   and _sources_hold(stats))
        tried_before = tried
        if settled:
            break
    else:
        raise Unsettled("the sources or their directories changed in each collection")
    names = _names(vals)
    entries, seen = [], set()
    for value, where in recorded:
        where = dict(where, name=_short_name(sorted(names[value])[0]))
        key = json.dumps(where, sort_keys=True)
        if key not in seen:
            seen.add(key)
            entries.append(where)

    def absolute(shown):
        return os.path.join(root, shown.lstrip("/")) if shown.startswith("/") else os.path.join(home, shown)

    return {"sources": [{"path": p, "size": s, "mtime_ns": m, "dev": d, "ino": i, "ctime_ns": c}
                        for p, (s, m, d, i, c) in sorted(stats.items())],
            "entries": entries, "missing": missing, "unreadable": unreadable, "accepted": accepted,
            "accepted_paths": [absolute(shown) for shown in accepted], "watch": watch, "probe": probe}


def read(home, root="/"):
    """Every value, and the sources that were missing, unreadable, or missing by acceptance.
    root: where the sources named by absolute path are looked for. "/" for every real use; a test
    gives its scratch directory, so that a scratch run reads nothing real (rev-g F8: without it
    every result, also a test's on made-up values, held the real values of those sources)."""
    vals, missing, unreadable, accepted = read_named(home, root)
    return set(vals), missing, unreadable, accepted


def read_named(home, root="/"):
    """As read(), with values a dict: value -> the names (source file and key) it was read from."""
    vals, missing, unreadable, accepted, _ = _collect(home, root)
    return _names(vals), missing, unreadable, accepted


def read_dead(home, root="/"):
    """The values read from nowhere but RETIRED, every one of whose lines there is marked dead.
    A value that any other source still holds is never dead, whatever the file says."""
    vals, _, _, _, dead = _collect(home, root)
    retired = {RETIRED, *_older_copies(home, RETIRED)}
    return {v for v, wheres in vals.items()
            if all(source in retired and key in dead for source, key, _ in wheres)}


def _collect(home, root):
    def under_root(path):
        return os.path.join(root, path.lstrip("/"))
    vals, missing, unreadable, accepted, dead = {}, [], [], [], set()

    def lines(path, shown):
        if _TRIED is not None:
            _TRIED.add(path)
        if not os.path.lexists(path):
            (accepted if shown in ACCEPTED_MISSING else missing).append(shown)
            return None
        try:
            with open(path, encoding="utf-8", errors="surrogateescape") as f:
                if _STAT is None:
                    return re.split("[\n\0]", f.read())
                # locate(): the stamp of the fd it is read from, taken before the read (R4, A77). A
                # change during the read moves it: the places' check (_held) and the last check by
                # path (_sources_hold) compare with this stamp, and collect again.
                _STAT[path] = _stamp(os.fstat(f.fileno()))
                return re.split("[\n\0]", f.read())
        except OSError:
            unreadable.append(shown)
            return None

    for rel in KV:
        got = lines(os.path.join(home, rel), rel)
        for number, line in enumerate(got or []):
            _context(os.path.join(home, rel), number, line)
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            name = key.replace("export ", "").strip()
            if name in NOT_SECRET:
                continue
            bare = value.strip().strip('"').strip("'")
            if name in BUCKET_KEYS and bare and all(BUCKET.fullmatch(b.strip()) for b in bare.split(",")):
                continue
            _add(vals, _kv_value(value), name, rel, name)
            _add(vals, value, name, rel, name)
            # A URL on the left of the first "=" (a line that is not KEY=VALUE): its password is a
            # value all the same, named by the file alone (rev-g N1).
            _add_passwords(vals, key, rel, "")
    for rel, keep in MOVED_TO_KEEP.items():
        if rel in missing and os.path.isfile(under_root(keep)) and os.path.getsize(under_root(keep)) > 0:
            missing.remove(rel)
            accepted.append(rel)
    for rel, older in ([(rel, older) for rel in KV for older in _older_copies(home, rel)]
                       + [(rel, shown) for rel in MIXED for shown in [rel] + _older_copies(home, rel)]):
        for number, line in enumerate(lines(os.path.join(home, older), older) or []):
            _context(os.path.join(home, older), number, line)
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            _add_credentials_only(vals, key.replace("export ", "").strip(), value, older)
            _add_passwords(vals, key, older, "")
    retired_by_name = {}
    for shown in [RETIRED] + _older_copies(home, RETIRED):
        for number, line in enumerate(lines(os.path.join(home, shown), shown) or []):
            _context(os.path.join(home, shown), number, line)
            line = line.strip()
            if not line or line.startswith("#") or "=" not in line:
                continue
            key, value = line.split("=", 1)
            name = key.replace("export ", "").strip()
            if name.startswith(DEAD_PREFIX):
                if shown == RETIRED:
                    dead.add(name[len(DEAD_PREFIX):])
                continue
            bare = _kv_value(value).strip().strip('"').strip("'")
            retired_by_name.setdefault(name, set()).add(bare)
            if len(bare) >= 8:
                vals.setdefault(bare, set()).add((shown, name, RETIRED_NOTE))
                _note(bare, _place(bare))
            _add_passwords(vals, bare, shown, name)
    dead = {name for name in dead if len(retired_by_name.get(name, ())) == 1}
    settings = [(os.path.join(home, rel), rel) for rel in SETTINGS_ENV]
    for pattern in SETTINGS_ENV_GLOBS:
        settings += [(path, os.path.relpath(path, home)) for path in sorted(glob.glob(os.path.join(home, pattern)))]
    for pattern in SETTINGS_ENV_GLOBS_ABSOLUTE:
        settings += [(path, "/" + os.path.relpath(path, root))
                     for path in sorted(glob.glob(under_root(pattern)))]
    for path, shown in settings:
        got = lines(path, shown)
        if got is None:
            continue
        try:
            doc = json.loads("\n".join(got))
        except ValueError:
            unreadable.append(shown)
            continue
        env = doc.get("env") if isinstance(doc, dict) else None
        for name, value in (env.items() if isinstance(env, dict) else []):
            if isinstance(name, str) and isinstance(value, str):
                _context(path, None, value, (sorted(doc).index("env"), sorted(env).index(name)))
                _add_credentials_only(vals, name, value, shown)
    for rel in WHOLE:
        for number, line in enumerate(lines(os.path.join(home, rel), rel) or []):
            _context(os.path.join(home, rel), number, line)
            _add(vals, line, None, rel, "")
        if rel in RENAMED and rel in missing and os.path.isfile(os.path.join(home, RENAMED[rel])):
            missing.remove(rel)
            accepted.append(rel)
        for older in _older_copies(home, rel):
            for number, line in enumerate(lines(os.path.join(home, older), older) or []):
                _context(os.path.join(home, older), number, line)
                _add(vals, line, None, older, "")
    for path in WHOLE_ABSOLUTE:
        for number, line in enumerate(lines(under_root(path), path) or []):
            _context(under_root(path), number, line)
            _add(vals, line, None, path, "")
    for rel, keep in MOVED_TO_KEEP.items():
        gave = any(source == rel for wheres in vals.values() for source, _, _ in wheres)
        if keep in missing and gave:
            missing.remove(keep)
            accepted.append(keep)
    for rel in JSON_LEAVES:
        got = lines(os.path.join(home, rel), rel)
        if got is not None:
            try:
                _leaves(json.loads("\n".join(got)), vals, rel, path=os.path.join(home, rel))
            except ValueError:
                unreadable.append(rel)
    return vals, missing, unreadable, accepted, dead


if __name__ == "__main__":
    print("secret-values.py is a module: it has no command and writes no value anywhere (see its header)",
          file=sys.stderr)
    sys.exit(2)
