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
which can delete the lock rules. The `cf` OAuth login in
`~/.config/cloudflare/` on this host is one, and it is readable by the same
uid the agents run as. To make the locks hold against this host too, log `cf`
out here when not administering Cloudflare (`cf auth logout`) and do bucket
administration from another machine.

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
| `~/.local/share/spawn-rsi/spawn.env` | `SPAWN_DATABASE_URL` (live) and `SPAWN_BACKUP_DRILL_DATABASE_URL` (the drill's server) | — |

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
that each `git bundle` verifies on its own, that the tar decrypts back to
the same SHA-256, and that the upload's ETag is the MD5 sent.

`spawn backup verify` is the drill (below). Exit 0 restores to the record; 1
does not (missing, corrupt, a sha256 or a count that differs, a failed
restore); 2 cannot run (no drill server named, no credentials, no key).

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
source database or any database it did not create.

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
every `bundles/incr/` object after it, in order (at most 6): decrypt each,
untar it, `git clone --mirror` each `repos/<name>.bundle` (an increment's
bundle fetches on top of the full one's, its prerequisites naming it), write
each `transcripts/<agent>/<file>` part at the offset its manifest records,
and `manifest.json` says which offset and how many bytes each part holds. A
full bundle fetches on its own.

## Cost

Measured on 2026-11-08 (this host, a scratch prefix and a scratch store, so no
real backup or record was touched; see the work record):

| What | Size | Time |
| --- | --- | --- |
| Full bundle: 122 transcript files (426,266,637 bytes) and both mission sources (spawn.git, supply-pa.git) | 89,767,892 bytes stored (tar + zstd + gpg) | 26 s |
| The same full bundle as a plain tar (before compression was added) | 437,032,960 bytes | 3 m 58 s |
| Increment: 2,000,000 bytes appended to one transcript | 19,209 bytes | 3 s |
| A dump of the live database (the last `pg-backup.sh` run, 2026-11-08) | 9,698,154 bytes | — |

At the owner's windows the bucket holds at most 43 dump copies (30 hourly, 8
daily, 5 weekly), 3 full bundles and 13 increments. A steady state at today's
sizes: 43 × 9.7 MB ≈ 417 MB of dumps, 3 × 89.8 MB ≈ 269 MB of full bundles,
and 13 increments, each a day of transcript growth (the measured 2 MB
appended in a day is 19 KB; even a day ten times that is under 200 KB, so
the increments are well under 10 MB together) — about 0.7 GB in all, 9 % of
the 8 GB alert (and of the 10 GB free tier). R2's free tier is 10 GB-month
storage, 1M Class A and 10M Class B operations a month, no egress fees;
lifecycle rules and bucket locks are free. Operations: the hourly dump is a
list plus 1-3 PUTs (Class A), 30 times a month; the bundle a list plus one
PUT, the drill a GET and a list; at most a few hundred Class A a month
against the million. The cheapest failure is not compression but the dump,
a whole copy of a log that only grows: it is what the alert watches. If the
tier were ever passed, storage is $0.015 per GB-month (about $0.011 a month
at 0.7 GB); egress is free, so a restore costs nothing.
