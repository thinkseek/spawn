# PostgreSQL backups to Cloudflare R2

Storage phase 3a ([docs/storage.md](../../docs/storage.md)). Once a day the
event log (the `spawn` database: `spawn_events`, `spawn_commands`, missions,
tasks, everything in it) is dumped, checked, encrypted on this host and
uploaded to R2, where **no key on this host can delete or overwrite it**
before it ages out.

| What | Where |
| --- | --- |
| Bucket | `spawn-store`, account q1293822641@gmail.com, location APAC, Standard class, private (no r2.dev URL, no custom domain) |
| Format | `pg_dump -Fc` by the live server's own major version (16, from its container image), then `gpg --symmetric` AES-256 (integrity-protected). pg_dump's compression only |
| Schedule | `spawn-backup.timer`: daily at 03:30 local + up to 45 min random delay; `Persistent=true` runs a missed day at next boot; a failed run is retried twice, 15 min apart (not a suspect one) |
| Local copy | newest good dump, unencrypted, `~/.local/share/spawn-rsi/backups/spawn-auto-<ts>.dump` (mode 600, directory 700); older `spawn-auto-*` are removed, manual dumps there are not |
| State | `~/.local/share/spawn-rsi/backups/pg-backup.state` (mode 600): the last good upload's key, `spawn_events` rows, size, ISO week, month |
| Scripts | `pg-backup.sh`, `pg-restore-drill.sh`, `lib.sh` here; the unit runs the installed copies in `~/.local/share/spawn-rsi/bin/backup/` |

## Layout, locks and expiry

Object keys are `<prefix>spawn-YYYYMMDD-HHMMSS.dump.gpg`, timestamp in UTC.
Retention is entirely R2's: the script never deletes.

| Prefix | Gets | Bucket lock (no delete, no overwrite) | Lifecycle expiry | Objects held |
| --- | --- | --- | --- | --- |
| `postgres/daily/` | every good backup | 14 days | 15 days | ~15 |
| `postgres/weekly/` | the first good backup of each ISO week (UTC) | 56 days | 60 days | ~9 |
| `postgres/monthly/` | the first good backup of each month (UTC) | 365 days | 400 days | ~13 |
| `postgres/suspect/` | dumps that failed the plausibility check | none | 30 days | normally 0 |
| `postgres/spawn-*` | the first two days' flat layout | none | 15 days | 1, until it expires |
| `locktest/` | throwaway probes to prove the lock works | 1 day | 2 days | 0-1 |

"First of the week/month" rather than "Sunday's/the 1st's": a host that was
off on that day still gets its weekly and monthly copy. The rules live in
`r2-lock-rules.json` and `r2-lifecycle-rules.json`; apply them with

```sh
cf r2 buckets locks update spawn-store --rules @deploy/backup/r2-lock-rules.json -f
cf r2 buckets lifecycle update spawn-store --rules @deploy/backup/r2-lifecycle-rules.json -f
cf r2 buckets locks get spawn-store; cf r2 buckets lifecycle get spawn-store
```

Each `update` replaces the whole rule set. Verified 2026-11-04 with the
host's S3 key: `DELETE` and an overwriting `PUT` of a locked object return
`409 ObjectLockedByBucketPolicy`; reading or writing the bucket's lifecycle
returns `403 AccessDenied`.

**What can still remove backups:** a Cloudflare login with account rights,
which can delete the lock rules. The `cf` OAuth login in
`~/.config/cloudflare/` on this host is one, and it is readable by the same
uid the agents run as. To make the locks hold against this host too, log `cf`
out here when not administering Cloudflare (`cf auth logout`) and do bucket
administration from another machine.

## Plausibility

The log is append-only. Before uploading, `pg-backup.sh` counts the
`spawn_events` rows *in the dump* and compares with the last good upload
(`pg-backup.state`). If the count fell, or the dump is under half the last
size, the dump goes to `postgres/suspect/` only, the state and every good
backup stay as they were, and the run exits 3 (systemd records the failure
and does not retry). Find out why; if the drop is intended (e.g. data was
deliberately removed), run `pg-backup.sh --accept-drop` once.

## Keys to keep safe

| File (mode 600) | Holds | If lost |
| --- | --- | --- |
| `~/.config/spawn/backup.key` | the encryption passphrase (32 random bytes, base64) | **every backup in R2 is unreadable.** Copy it off this host now (password manager or paper) |
| `~/.config/spawn/r2.env` | `SPAWN_R2_ENDPOINT`, `SPAWN_R2_BUCKET`, `SPAWN_R2_ACCESS_KEY_ID`, `SPAWN_R2_SECRET_ACCESS_KEY` | make a new token (below) |
| `~/.local/share/spawn-rsi/spawn.env` | `SPAWN_DATABASE_URL`, the live database | — |

The R2 credentials are the S3 form of a Cloudflare user API token named
`spawn-store-backup` with only *Workers R2 Storage Bucket Item Read/Write* on
bucket `spawn-store` (access key id = token id, secret = SHA-256 of the token
value); they cannot see other buckets or change bucket settings, and the
locks stop them deleting backups. To replace them: revoke the token
(dashboard, My Profile, API Tokens), create one the same way
(`cf user tokens create --policies ...` with the two permission groups and
resource `com.cloudflare.edge.r2.bucket.<account id>_default_spawn-store`),
and rewrite `r2.env`.

Nothing here prints a secret: database passwords reach libpq through
`PGPASSWORD` or a `docker --env-file` on a file descriptor, R2 keys reach
curl through a config on a file descriptor, so neither is in `ps` or the
journal.

## What a run checks

`pg-backup.sh` exits non-zero, with the reason, if any of these fail: the
dump; `pg_restore -l` reads it and it holds `spawn_events` data; the
plausibility check (exit 3); the encrypted file decrypts back to the same
SHA-256; the size is under R2's 5 GiB single-PUT limit (a warning from 4 GB);
R2's ETag equals the MD5 of what was sent, for each copy. Each object carries
the dump's SHA-256 and `spawn_events` count as `x-amz-meta-sha256` and
`x-amz-meta-spawn-events`.

## Install

```sh
install -d ~/.local/share/spawn-rsi/bin/backup
install -m 644 deploy/backup/lib.sh ~/.local/share/spawn-rsi/bin/backup/
install -m 755 deploy/backup/pg-backup.sh deploy/backup/pg-restore-drill.sh ~/.local/share/spawn-rsi/bin/backup/
deploy/backup/pg-backup.sh --init-key        # only if backup.key does not exist yet
install -m 644 deploy/systemd/spawn-backup.{service,timer} ~/.config/systemd/user/
systemctl --user daemon-reload && systemctl --user enable --now spawn-backup.timer
systemctl --user start spawn-backup.service && journalctl --user -u spawn-backup -n 20
```

## Restore drill

`pg-restore-drill.sh [KEY]` takes the newest `postgres/daily/` backup (or
KEY), decrypts it, checks the SHA-256 recorded at upload, and restores it with
`pg_restore` inside a throwaway container of the live server's image. The
container has no network (`--network none`; reached only by `docker exec`),
keeps its data on tmpfs (`SPAWN_DRILL_TMPFS_SIZE`, default 4g), is labelled
`spawn.restore-drill` and removed on exit; leftovers of earlier drills are
removed at start. It prints the row counts of `spawn_events`,
`spawn_commands`, `mission_missions`, `mission_tasks` beside live's
(read-only transactions) and checks `spawn_events` against the count recorded
at upload. It refuses if the drill server has the live server's system
identifier or postmaster start time. Nothing reaches the shared test server.
Run it after any change here and now and then.

## Restoring for real

Stop what writes to the database first (`systemctl --user stop spawn-loop
spawn-integrate spawn-serve`), and restore into a **new** database, never over
the live one, then point `SPAWN_DATABASE_URL` at it.

```sh
# 1. fetch: list, take the newest key (or the local spawn-auto-*.dump and skip 2)
set -a; . ~/.config/spawn/r2.env; set +a
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET?list-type=2&prefix=postgres%2Fdaily%2F"
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  -o spawn.dump.gpg "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET/postgres/daily/spawn-YYYYMMDD-HHMMSS.dump.gpg"
# 2. decrypt
gpg --batch --pinentry-mode loopback --passphrase-file ~/.config/spawn/backup.key -o spawn.dump -d spawn.dump.gpg
# 3. restore into a new, empty database (pg_restore 16 or newer)
createdb ... spawn_restored
pg_restore --no-owner -d '<url of spawn_restored>' spawn.dump
```

(`--user` on the command line shows the key in `ps`; fine for a one-off by
hand, which is why the scripts do not do it.) The one flat-layout object,
`postgres/spawn-20261104-130240.dump.gpg` (expires ~2026-11-19), was made by
pg_dump 18 and needs pg_restore 17+; everything under `daily/`, `weekly/`,
`monthly/` is a PostgreSQL 16 archive.

## Cost

Inside R2's free tier (10 GB-month storage, 1M Class A and 10M Class B
operations a month, no egress fees; lifecycle rules and bucket locks are
free). A backup is about 0.27 MB today and ~37 objects are held (15 daily, 9
weekly, 13 monthly): about 10 MB. A run is 1-3 PUTs (Class A); no list, no
delete. A drill is 1 ListObjectsV2 (Class A) and 1 GET (Class B). About 35
Class A operations a month. At 100 MB per dump the store would hold ~3.7 GB.
