# The fleet's record off this host: dumps and bundles to Cloudflare R2

The off-host copy of everything Spawn cannot rebuild
([docs/storage.md](../../docs/storage.md), "Where state lives"): the event log
every hour (an encrypted `pg_dump`), and once a day the mission sources and
the transcript archive (an encrypted tar of git bundles and transcript
files). Both are encrypted on this host before they leave it, and **no key
on this host can delete or overwrite them** in R2 before they age out.

The command is the binary's: `spawn backup dump|bundle|verify`
(`crates/spawn-cli/src/backup.rs`). This directory holds what it does not:
the timers, the bucket's lifecycle and lock rules, the old shell tools
(`pg-backup.sh`, `pg-restore-drill.sh`, `lib.sh`) kept for manual use, and
the drill's test.

| What | Where |
| --- | --- |
| Bucket | `spawn-store`, account q1293822641@gmail.com, location APAC, Standard class, private (no r2.dev URL, no custom domain) |
| Format | dumps: `pg_dump -Fc` (compression `SPAWN_BACKUP_DUMP_COMPRESS`, default `zstd:9`) and `gpg --symmetric` AES-256; bundles: a tar (zstd) of `manifest.json`, `repos/*.bundle` and `transcripts/`, the same `gpg` passphrase |
| Schedule | `spawn-backup.timer` (hourly, `spawn backup dump`), `spawn-backup-bundle.timer` (daily 04:00, `spawn backup bundle`), `spawn-backup-verify.timer` (daily 05:00, `spawn backup verify`); all `Persistent=true`, so a run missed while the host was off happens at next boot |
| Records | every run writes a `backup.dumped`, `backup.bundled` or `backup.verified` control record (owner-only `spawn_backup_record`); the tiers, the bundle's base and the plausibility check are read from them, never from a state file |
| Settings | `SPAWN_BACKUP_KEY_FILE`, `SPAWN_R2_ENV_FILE`, `SPAWN_BACKUP_R2_LIMIT_MB` (10000), `SPAWN_BACKUP_R2_ALERT_MB` (8000), `SPAWN_BACKUP_TRANSCRIPT_DAYS` (14), `SPAWN_BACKUP_DRILL_DATABASE_URL` (the drill's disposable server), `SPAWN_BACKUP_PREFIX` (empty in the fleet; a test run's `locktest/`) |

## Layout, locks and expiry

Object keys are `postgres/<tier>/spawn-YYYYMMDD-HHMMSS.dump.gpg` and
`bundles/<full|incr>/spawn-bundle-YYYYMMDD-HHMMSS.tar.gpg`, timestamps in
UTC. Retention is entirely R2's: nothing here ever deletes or overwrites.

| Prefix | Gets | Bucket lock | Lifecycle expiry | Held at most |
| --- | --- | --- | --- | --- |
| `postgres/hourly/` | every good dump | 5 h | 6 h | 30 |
| `postgres/daily/` | the UTC day's first good dump | 6 d | 7 d | 8 |
| `postgres/weekly/` | the ISO week's first good dump | 27 d | 28 d | 5 |
| `postgres/suspect/` | dumps that failed the plausibility check | none | 30 d | normally 0 |
| `bundles/full/` | the ISO week's first bundle, or a bundle with no full one within 7 days | 13 d | 14 d | 3 |
| `bundles/incr/` | every other bundle, against its base | 13 d | 14 d | 13 |
| `postgres/monthly/` | the old monthly copies, no longer written | (as before) | 400 d | ages out |
| `postgres/spawn-*` | the flat layout the first backups used | none | 15 d | ages out |
| `locktest/` | throwaway probes to prove the lock works | 1 d | 2 d | 0-1 |

"First of the day/week" rather than "midnight's/Sunday's": a host that was
off on that day still gets its daily and weekly copy. The windows are the
owner's (2026-11-06 ruling: stay inside R2's free tier, no monthly tier, the
bundles no longer grow with the archive). The rules live in
`r2-lock-rules.json` and `r2-lifecycle-rules.json`; apply them with

```sh
cf r2 buckets locks update spawn-store --rules @deploy/backup/r2-lock-rules.json -f
cf r2 buckets lifecycle update spawn-store --rules @deploy/backup/r2-lifecycle-rules.json -f
cf r2 buckets locks get spawn-store; cf r2 buckets lifecycle get spawn-store
```

Each `update` replaces the whole rule set. Tested with the host's S3 key:
`DELETE` and an overwriting `PUT` of a locked object return
`409 ObjectLockedByBucketPolicy`; reading or writing the bucket's lifecycle
returns `403 AccessDenied`.

**What can still remove backups:** a Cloudflare login with account rights,
which can change or delete the lock and lifecycle rules. None is on this
host: there is no `cf` binary and no `~/.config/cloudflare/`, the bucket's
S3 key answers `403` to the lifecycle (above), and the key is the S3 form of
a token limited to bucket item read/write, which cannot change bucket
settings at all. So the rules are applied from the owner's machine (the `cf`
commands above). What that holds: no process running as this host's uid can
delete or edit the lock rules, so a locked object cannot be deleted from here
before its lock runs out (`DELETE` and an overwriting `PUT` answer
`409 ObjectLockedByBucketPolicy`). What it does not hold: the same uid can
read every object, and the passphrase in `~/.config/spawn/backup.key` is its
to read, so a compromised host can take the backups with it; and it can
delete what the locks do not cover — an object whose lock has lapsed, or
`postgres/suspect/`, which has no lock rule. The locks bound deletion, not
reading; a missing dump or bundle fails the daily `verify` (exit 1) and
raises the `backup.verified` alert afterwards.

## The limit and the alert

`SPAWN_BACKUP_R2_LIMIT_MB` (default 10000: 10,000,000,000 bytes, the free
tier in decimal GB) and `SPAWN_BACKUP_R2_ALERT_MB` (default 8000). Every run
lists the whole bucket under `<root>/.backup.lock` before its first PUT and
writes no copy that would pass the limit — the hourly dump included: a dump
whose hourly copy was skipped exits 4, so the unit fails and
`systemctl --user --failed` names it. Past the alert the record says
`over_alert` and the run says it once on its output; objective 030 raises the
`backup_budget` alert from that record. A value that does not parse, or an
alert above the limit, is refused (exit 2, recorded with its reason).

## Plausibility

The log is append-only. Before uploading, `spawn backup dump` counts the
`spawn_events` rows *in the dump* and compares with the newest good
`backup.dumped` record. If the count fell, or the dump is under half the
last size, the dump goes to `postgres/suspect/` only, the good backups and
the records stay as they were, and the run exits 3 (the unit records the
failure and is told not to retry). Find out why; if the drop is intended
(e.g. data was deliberately removed), accept it once by hand:
`SPAWN_BACKUP_ACCEPT_DROP=1` (the old `pg-backup.sh --accept-drop`).

## Keys to keep safe

| File (mode 600) | Holds | If lost |
| --- | --- | --- |
| `~/.config/spawn/backup.key` | the encryption passphrase (32 random bytes, base64) | **every dump and bundle in R2 is unreadable.** Copy it off this host now (password manager or paper) |
| `~/.config/spawn/r2.env` | `SPAWN_R2_ENDPOINT`, `SPAWN_R2_BUCKET`, `SPAWN_R2_ACCESS_KEY_ID`, `SPAWN_R2_SECRET_ACCESS_KEY` | make a new token (below) |
| `~/.local/share/spawn-rsi/spawn.env` | `SPAWN_DATABASE_URL` (live) and `SPAWN_BACKUP_DRILL_DATABASE_URL` (the fleet test database's server; the drill makes and drops its own database there) | — |

The R2 credentials are the S3 form of a Cloudflare user API token named
`spawn-store-backup` with only *Workers R2 Storage Bucket Item Read/Write* on
bucket `spawn-store` (access key id = token id, secret = SHA-256 of the token
value); they cannot see other buckets or change bucket settings, and the
locks stop them deleting backups. To replace them: revoke the token
(dashboard, My Profile, API Tokens), create one the same way
(`cf user tokens create --policies ...` with the two permission groups and
resource `com.cloudflare.edge.r2.bucket.<account id>_default_spawn-store`),
and rewrite `r2.env`.

Nothing here prints a secret: the passphrase reaches gpg by path, the R2 keys
reach curl on stdin (`--config -`), the database password reaches libpq
through the environment, and none of them is on a command line, in the
output or in a record.

## What a run checks

`spawn backup dump` exits non-zero, with the reason, if: the dump fails,
`pg_restore -l` cannot read it or it holds no `spawn_events` data, the
plausibility check fails (exit 3), the encrypted file does not decrypt back
to the same SHA-256, the size is over R2's 5 GiB single-PUT limit, or R2's
ETag is not the MD5 of what was sent. Each object carries the dump's SHA-256
and `spawn_events` count as `x-amz-meta-sha256` and
`x-amz-meta-spawn-events`.

`spawn backup bundle` checks that every mission source is a repository on
this host (one that is not is recorded `missing` and the bundle goes on),
that each `git bundle` verifies on its own, that the archive decrypts back
to the same SHA-256 (the archive, not the tar: compression is before
encryption), and that the upload's ETag is the MD5 sent.

A source's bundle holds its branches and tags, exactly the refs the record
names (`refs/heads/*` and `refs/tags/*`, in each repo's `heads`), and nothing
else: not a remote-tracking ref a mirror clone left (the live
`missions/supply-pa.git` carries `refs/remotes/github/main`), not a pending
`refs/spawn-owner/*` push. Those are not the mission's history, and a record
that named neither could not be a true description of an archive that held
them — the drill refuses any ref the manifest does not name (below). A
source with no branches and no tags (a mission added before its first push)
is recorded `empty`, `heads` `{}` and no file in the archive: `git bundle
create` will not make an empty bundle, there is nothing of the mission to
hold, and a restore of it makes an empty repository.

`spawn backup verify` is the drill (below): the newest good dump restores to
its record, and the newest bundle is opened and compared with its own. Exit
0 both pass; 1 either does not (missing, corrupt, a sha256 or a count that
differs, a failed restore, a bundle that does not open or verify); 2 cannot
run (no drill server named, no credentials, no key).

## Install

The binary is installed as `bin/spawn` by `deploy/deploy.sh`, like every
other command; the timers are new here. After `spawn backup` is in the
deployed `bin/spawn`:

```sh
install -m 644 deploy/systemd/spawn-backup.service deploy/systemd/spawn-backup.timer \
  deploy/systemd/spawn-backup-bundle.service deploy/systemd/spawn-backup-bundle.timer \
  deploy/systemd/spawn-backup-verify.service deploy/systemd/spawn-backup-verify.timer \
  ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user disable --now spawn-backup.timer 2>/dev/null || true   # the old daily script's
systemctl --user enable --now spawn-backup.timer spawn-backup-bundle.timer spawn-backup-verify.timer
systemctl --user start spawn-backup.service && journalctl --user -u spawn-backup -n 20
```

`SPAWN_BACKUP_DRILL_DATABASE_URL` must be in `spawn.env` before the drill
timer is enabled: a disposable server (the fleet's test database), never the
live one. The drill refuses without it, by name, and touches nothing.

**What changed in the old install:** `spawn-backup.timer` ran
`pg-backup.sh` once a day at 03:30; it now runs `spawn backup dump` every
hour, and the two new timers add the daily bundle and the daily drill.
`pg-backup.sh` and `pg-restore-drill.sh` stay in `bin/backup/` for manual
use, but nothing runs them on a schedule any more (running both would write
the same tiers twice). The pre-existing `postgres/monthly/` objects still
expire under their old rule and no new ones are written.

## Restore drill

`spawn backup verify` (daily, `spawn-backup-verify.timer`) takes the newest
good `backup.dumped` record, fetches the object of its first key, checks the
object's `x-amz-meta-sha256` and the decrypted dump's SHA-256 against the
record's (gpg's exit status counts: a corrupted object can decrypt to bytes
with a warning and exit 2), creates a database of its own
(`spawn_drill_<unix time>_<random>`) on the server of
`SPAWN_BACKUP_DRILL_DATABASE_URL`, restores into it with no error ignored,
counts `spawn_events` and their highest id there, compares both with the
record's, and drops the database on every path. It never restores into the
source database or any database it did not create: `SPAWN_BACKUP_DRILL_DATABASE_URL`
names the fleet test database's server — the database in the URL is the one
the drill connects to for its `create` and `drop`, not a restore target — so
its login needs `CREATEDB` and nothing more, a scratch name that already
exists is refused, and the drop is only ever of the database the drill made.

It then opens the newest `backup.bundled` record with a key — the sources
and transcripts, the other half of what leaves the host — the same way:
fetches its object, checks `x-amz-meta-sha256` and the decrypted archive's
SHA-256 against the record's, decompresses it by its magic, untars it, and
compares `manifest.json` with the record (kind, transcript file count and
bytes), every transcript file it names at the recorded size, and every
repo's bundle: `git bundle list-heads` must list every head the manifest
names and nothing it does not — for an increment, the heads that are new or
moved since its base, since `--not` leaves an unmoved head out — and a full
bundle must additionally pass `git bundle verify` in an empty repository
(an increment's prerequisites are its base's commits, which the drill does
not fetch). A store with no bundle yet says so in the record and passes. The result is the record's `bundle`
object; a bundle that fails makes the day's verdict a failure.

`pg-restore-drill.sh [KEY]` remains for the manual, docker-based drill the
old daily script documented: a throwaway container of the live server's
image, no network, tmpfs data, the row counts beside live's. Run it after any
change here, and periodically. `test-pg-restore-drill.sh` tests it without
R2 or the live database.

## Restoring for real

Stop what writes to the database first (`systemctl --user stop spawn-loop
spawn-integrate spawn-serve`), and restore into a **new** database, never over
the live one, then point `SPAWN_DATABASE_URL` at it.

```sh
# 1. fetch: list, take the newest key under postgres/daily/
set -a; . ~/.config/spawn/r2.env; set +a
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET?list-type=2&prefix=postgres%2Fdaily%2F"
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  -o spawn.dump.gpg "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET/postgres/daily/spawn-YYYYMMDD-HHMMSS.dump.gpg"
# 2. decrypt and restore into a new, empty database
gpg --batch --pinentry-mode loopback --passphrase-file ~/.config/spawn/backup.key -o spawn.dump -d spawn.dump.gpg
createdb ... spawn_restored
pg_restore --no-owner -d '<url of spawn_restored>' spawn.dump
```

(`--user` on the command line shows the key in `ps`; fine for a one-off by
hand, which is why the command does not do it.)

The sources and transcripts come from the newest `bundles/full/` object and
every `bundles/incr/` object after it, in order (at most 6): decrypt each, and
unpack it by its magic — the key names what it holds, and the first bytes say
the same:

```sh
gpg --batch --pinentry-mode loopback --passphrase-file ~/.config/spawn/backup.key \
  -o bundle.raw -d spawn-bundle-YYYYMMDD-HHMMSS.tar.zst.gpg
case "$(head -c 4 bundle.raw | od -An -tx1 | tr -d ' \n')" in
1f8b*)     gzip -dc <bundle.raw >bundle.tar ;;
28b52ffd*) zstd -q -dc <bundle.raw >bundle.tar ;;
*)         cp bundle.raw bundle.tar ;;   # plain
esac
mkdir -p unpacked && tar -xf bundle.tar -C unpacked
```

Then `git clone --mirror` each `unpacked/repos/<name>.bundle` (an increment's
bundle fetches on top of the full one's, its prerequisites naming it), write
each `transcripts/<agent>/<file>` part at the offset its manifest records,
and `manifest.json` says which offset and how many bytes each part holds. A
full bundle fetches on its own.

## Cost

Measured on 2026-11-08 (this host, a scratch prefix and a scratch store, so no
real backup or record was touched; see the work record):

| What | Size | Time |
| --- | --- | --- |
| Full bundle: 122 transcript files (426,266,637 bytes) and both mission sources (spawn.git, supply-pa.git) | 89,767,892 bytes stored (tar + zstd + gpg); zstd -3 measured 7.7:1 on real transcript text | 26 s |
| The same full bundle as a plain tar (before compression was added) | 437,032,960 bytes | 3 m 58 s |
| Increment: 2,000,000 bytes appended to a real transcript | 260,948 bytes (zstd -3; 323,913 with gzip -6) | 3 s |
| A dump of the live database (the last `pg-backup.sh` run, 2026-11-08) | 9,698,154 bytes | — |

The increment figure is a correction of the first measurement: 2,000,000
bytes of 031-style synthetic JSONL compress to 19,209 bytes, but real
transcript text does not — the same 2 MB of a real transcript's tail is
260,948 bytes, 13 times more. The earlier estimate of a day's growth used
the synthetic number; this one does not.

At the owner's lifecycle windows the bucket holds one dump object per tier
per run (the hourly always, the daily the first of the UTC day, the weekly
the first of the ISO week), and the 6 h / 7 d / 28 d windows keep about 7
hourly, 8 daily and 5 weekly copies: ≈ 20 dump objects ≈ 194 MB — the
windows decide the count, not how often the dump runs. With 3 full bundles
(≈ 269 MB) and up to 13 increments of a real day's growth (MB-scale each —
the live archive is 428,849,784 bytes and grew tens of MB in the last day —
so tens of MB up to ≈ 150 MB together), a steady state is ≈ 0.5–0.6 GB in
all: about 7 % of the 8 GB alert and 6 % of
the 10 GB free tier. The earlier ≈ 0.7 GB / 9 % estimate stands by
correction, not by luck: its dump line was about twice too high (417 MB for
43 daily copies, against ≈ 20 tier objects at ≈ 194 MB) and its increment
line about ten times too low (under 10 MB, against tens to ≈ 150 MB); the
two errors happened to cancel inside the same conclusion, and now the
conclusion is the corrected numbers' own — the bucket is nowhere near the
alert or the tier.

R2's free tier is 10 GB-month storage, 1M Class A and 10M Class B operations
a month, no egress fees; lifecycle rules and bucket locks are free.
Operations: the hourly dump runs about 730 times a month, a list plus 1–3
PUTs each (the daily and weekly copies included), ≈ 1,500–2,200 Class A a
month; the bundle a list plus one PUT, the drill a GET and a list — at most
a few thousand Class A against the million. The cheapest failure is not
compression but the dump, a whole copy of a log that only grows: it is what
the alert watches. If the tier were ever passed, storage is $0.015 per
GB-month (about $0.01 a month at 0.6 GB); egress is free, so a restore costs
nothing.
