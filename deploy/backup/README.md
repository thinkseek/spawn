# PostgreSQL backups to Cloudflare R2

Storage phase 3a ([docs/storage.md](../../docs/storage.md)). Once a day the
event log (the `spawn` database: `spawn_events`, `spawn_commands`, missions,
tasks, everything in it) is dumped, encrypted on this host and uploaded to R2.

| What | Where |
| --- | --- |
| Bucket | `spawn-store`, account q1293822641@gmail.com, location APAC, Standard class, private (no r2.dev URL, no custom domain) |
| Objects | `postgres/spawn-YYYYMMDD-HHMMSS.dump.gpg`, timestamp in UTC |
| Format | `pg_dump -Fc` (compressed by pg_dump, nothing else compresses it), then `gpg --symmetric` AES-256 (integrity-protected) |
| Retention | newest backup of each of the newest 14 days, plus of each of the newest 8 ISO weeks: about 19 objects |
| Local copy | newest dump, unencrypted, `~/.local/share/spawn-rsi/backups/spawn-auto-<ts>.dump` (mode 600, directory 700); older `spawn-auto-*` are removed, manual dumps there are not |
| Schedule | `spawn-backup.timer`: daily at 03:30 local + up to 45 min random delay; `Persistent=true` runs a missed day at next boot; a failed run is retried twice, 15 min apart |
| Scripts | `pg-backup.sh`, `pg-restore-drill.sh`, `lib.sh` here; the units run the installed copies in `~/.local/share/spawn-rsi/bin/backup/` |

## Keys to keep safe

| File (mode 600) | Holds | If lost |
| --- | --- | --- |
| `~/.config/spawn/backup.key` | the encryption passphrase (32 random bytes, base64) | **every backup in R2 is unreadable.** Copy it off this host now (password manager or paper) |
| `~/.config/spawn/r2.env` | `SPAWN_R2_ENDPOINT`, `SPAWN_R2_BUCKET`, `SPAWN_R2_ACCESS_KEY_ID`, `SPAWN_R2_SECRET_ACCESS_KEY` | make a new token (below) |
| `~/.local/share/spawn-rsi/spawn.env` | `SPAWN_DATABASE_URL`, the live database | — |
| `~/.config/spawn/test-database.url` | the throwaway test server, used by the drill only | — |

The R2 credentials are the S3 form of a Cloudflare user API token named
`spawn-store-backup` with only *Workers R2 Storage Bucket Item Read/Write* on
bucket `spawn-store` (access key id = token id, secret = SHA-256 of the token
value); they cannot see other buckets. To replace them: revoke the token
(dashboard, My Profile, API Tokens), create one the same way
(`cf user tokens create --policies ...` with the two permission groups and
resource `com.cloudflare.edge.r2.bucket.<account id>_default_spawn-store`),
and rewrite `r2.env`.

Nothing here prints a secret: database passwords reach libpq through
`PGPASSWORD`, R2 keys reach curl through a config on a file descriptor
(`curl --aws-sigv4`), so neither is in `ps` or the journal.

## What a run checks

`pg-backup.sh` exits non-zero, with the reason, if any of these fail: the
dump; `pg_restore -l` reads it and it holds `spawn_events` data; the encrypted
file decrypts back to the same SHA-256; R2's ETag equals the MD5 of what was
sent; the new object is in the listing. Only then does it prune, from a single
ListObjectsV2 call, and never the object it just wrote; objects under
`postgres/` with other names are left alone.

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

`pg-restore-drill.sh` downloads the newest backup, decrypts it, checks the
SHA-256 recorded at upload, restores it into a new database
`spawn_drill_<ts>` on the **test** server (`test-database.url`), prints the row
counts of `spawn_events`, `spawn_commands`, `mission_missions`, `mission_tasks`
beside live's (read-only transactions), and drops the scratch database
(`--keep` leaves it). It refuses to run if the test URL names port 15433, equals
the live URL, or reaches the same running server as live. Run it after any
change here and now and then.

## Restoring for real

Stop what writes to the database first (`systemctl --user stop spawn-loop
spawn-integrate spawn-serve`), and restore into a **new** database, never over
the live one, then point `SPAWN_DATABASE_URL` at it.

```sh
# 1. fetch: newest is the last key in the listing (or take the local spawn-auto-*.dump and skip 2)
set -a; . ~/.config/spawn/r2.env; set +a
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET?list-type=2&prefix=postgres%2F"
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  -o spawn.dump.gpg "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET/postgres/spawn-YYYYMMDD-HHMMSS.dump.gpg"
# 2. decrypt
gpg --batch --pinentry-mode loopback --passphrase-file ~/.config/spawn/backup.key -o spawn.dump -d spawn.dump.gpg
# 3. restore into a new, empty database
createdb ... spawn_restored
pg_restore --no-owner -f - spawn.dump | sed '0,/^SET transaction_timeout = 0;$/{//d}' \
  | psql -1 -v ON_ERROR_STOP=1 -d '<url of spawn_restored>'
```

The `sed` is needed because pg_restore 17+ (this host has 18) opens with
`SET transaction_timeout`, which the PostgreSQL 16 server rejects; with a
pg_restore of the server's own version, `pg_restore --no-owner -d <url>
spawn.dump` works directly. (`--user` on the command line shows the key in
`ps`; fine for a one-off by hand, which is why the scripts do not do it.)

## Cost

Inside R2's free tier (10 GB-month storage, 1M Class A and 10M Class B
operations a month, no egress fees). A backup is about 0.25 MB today; ~19
objects is a few MB. A run is 2 Class A operations (PUT, ListObjectsV2) and
free DeleteObjects; a drill is 1 Class A and 1 Class B. Even at 100 MB per
dump the store stays near 2 GB.
