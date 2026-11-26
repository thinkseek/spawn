#!/bin/bash
# Cases for scripts/redact/secret-values.py (issue 98): Keep first, the plaintext file as fallback.
# Everything is made up and lives in a scratch folder given to the module as its home and root, so
# it reads nothing real (no /run/keep, no ~/.config/spawn): synthetic strings only, never printed.
#
#   bash scripts/redact/test-secret-values.sh
#
# Covered: Keep's file alone (the plaintext file deleted) is no refusal; both there load both;
# the plaintext file alone (Keep not delivered) is no refusal; both gone, or one gone and the other
# empty (a plaintext file with no value: empty, comments only, KEY=), refuse; locate() places a Keep value; the module still has no command and its lists name
# the live Keep pair; and credentials.manifest's rows agree with KEEP.
set -u
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
module=$here/secret-values.py
scratch=$(mktemp -d) || exit 2
[ -n "$scratch" ] && [ -d "$scratch" ] || exit 2
case $scratch in "$HOME"|"$HOME"/.config*|/run/keep*) echo "refusing scratch $scratch"; exit 2 ;; esac
trap 'rm -rf -- "$scratch"' EXIT

out=$(python3 -I "$module" 2>&1); st=$?
fails=0
if [ "$st" = 2 ] && [[ $out == *"is a module"* ]]; then echo "ok   run as a command: exit 2, it is a module"
else echo "FAIL run as a command"; fails=1; fi

python3 -I -B - "$module" "$scratch" "$repo/scripts/keep/credentials.manifest" <<'PY' || fails=$((fails + 1))
import importlib.util, os, shutil, sys

module, scratch, manifest = sys.argv[1:4]
spec = importlib.util.spec_from_file_location("secret_values", module)
sv = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sv)
home, root = os.path.join(scratch, "home"), os.path.join(scratch, "root")
PLAIN, KEEP = ".config/spawn/typesafe.env", "/run/keep/typesafe.api-key"
KEEP_VALUE, PLAIN_VALUE = "synthetic-keep-value-0001", "synthetic-plain-value-0002"
fails = 0


def case(name, ok):
    global fails
    print(("ok   " if ok else "FAIL ") + name)
    fails += 0 if ok else 1


def setup(keep=None, plain=None, plain_raw=None):
    """keep/plain: None (absent), "" (empty) or a made-up value; plain_raw: the plaintext file's
    whole text instead."""
    shutil.rmtree(home, ignore_errors=True)
    shutil.rmtree(root, ignore_errors=True)
    os.makedirs(os.path.join(home, ".config/spawn"))
    os.makedirs(os.path.join(root, "run/keep"))
    if keep is not None:
        with open(os.path.join(root, KEEP.lstrip("/")), "w") as f:
            f.write(keep)
    if plain is not None:
        with open(os.path.join(home, PLAIN), "w") as f:
            f.write(plain and "TYPESAFE_API_KEY=" + plain + "\n")
    if plain_raw is not None:
        with open(os.path.join(home, PLAIN), "w") as f:
            f.write(plain_raw)
    return sv.read(home, root)


vals, missing, unreadable, accepted = setup(keep=KEEP_VALUE)
case("Keep only (plaintext deleted): its value read, the plaintext missing by acceptance, no refusal for the pair",
     KEEP_VALUE in vals and PLAIN in accepted and PLAIN not in missing and KEEP not in missing and KEEP not in accepted)
vals, missing, unreadable, accepted = setup(keep=KEEP_VALUE, plain=PLAIN_VALUE)
case("both there: both values read, neither missing nor accepted",
     {KEEP_VALUE, PLAIN_VALUE} <= vals and not {PLAIN, KEEP} & set(missing + accepted))
vals, missing, unreadable, accepted = setup(plain=PLAIN_VALUE)
case("plaintext only (Keep not delivered): its value read, Keep's file missing by acceptance",
     PLAIN_VALUE in vals and KEEP in accepted and KEEP not in missing and PLAIN not in missing)
vals, missing, unreadable, accepted = setup()
case("both gone: both refuse (missing, not accepted)",
     PLAIN in missing and KEEP in missing and not {PLAIN, KEEP} & set(accepted))
vals, missing, unreadable, accepted = setup(keep="")
case("Keep's file empty, plaintext gone: the plaintext refuses", PLAIN in missing and PLAIN not in accepted)
vals, missing, unreadable, accepted = setup(plain="")
case("plaintext empty, Keep's file gone: Keep's file refuses", KEEP in missing and KEEP not in accepted)
vals, missing, unreadable, accepted = setup(plain_raw="# a made-up comment, no value\n\n")
case("plaintext with comments only, Keep's file gone: Keep's file refuses (no value)",
     KEEP in missing and KEEP not in accepted)
vals, missing, unreadable, accepted = setup(plain_raw="TYPESAFE_API_KEY=\n")
case("plaintext with an empty KEY=, Keep's file gone: Keep's file refuses (no value)",
     KEEP in missing and KEEP not in accepted)

setup(keep=KEEP_VALUE)
named = sv.read_named(home, root)[0]
case("read_named names the Keep value by its path", named.get(KEEP_VALUE) == {KEEP})
where = sv.locate(home, root)
placed = [e for e in where["entries"] if e["path"] == os.path.join(root, KEEP.lstrip("/"))]
case("locate() places the Keep value and gives the plaintext's accepted path",
     len(placed) == 1 and placed[0]["span"] == [0, len(KEEP_VALUE)]
     and os.path.join(home, PLAIN) in where["accepted_paths"] and KEEP_VALUE not in repr(where))
case("the lists keep the live shape: WHOLE_ABSOLUTE ends with Keep's file, MOVED_TO_KEEP maps typesafe.env to it",
     sv.WHOLE_ABSOLUTE[-1] == KEEP and sv.MOVED_TO_KEEP == {PLAIN: KEEP} and callable(sv.credential_name))

rows = {}
for line in open(manifest):
    if line.strip() and not line.lstrip().startswith("#"):
        f = [x.strip() for x in line.split("|")]
        rows[f[0]] = f[4]
case("credentials.manifest agrees with KEEP: each id, and its legacy_file as ~/<KEEP's file>",
     rows == {k: "~/" + rel if rel else "-" for k, rel in sv.KEEP.items()})
print(f"{fails} failed (secret-values)")
sys.exit(1 if fails else 0)
PY

emit_out=$(python3 -I -B "$here/emit-locations.py" "$scratch/home" "$scratch/root")
export emit_out
python3 - <<'PY'
import json, os, sys
raw = os.environ["emit_out"]
data = json.loads(raw)
assert isinstance(data.get("entries"), list) and len(data["entries"]) > 0, data
for secret in ["sk-test-0001-secret", "ghp_test_0002_secret", "postgres://user:pass@host/db"]:
    assert secret not in raw, f"secret leaked: {secret}"
print("emit-locations.py: ok")
PY

echo "$fails failed"
[ "$fails" -eq 0 ]
