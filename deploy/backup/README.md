# The fleet's record off this host: dumps and bundles to Cloudflare R2

The off-host copy of everything Spawn cannot rebuild
([docs/storage.md](../../docs/storage.md), "Where state lives"): the event log
every hour (an encrypted `pg_dump`), and once a day the mission sources and
the transcript archive (an encrypted tar of git bundles and transcript
files). Both are encrypted on this host before they leave it, and **no key
on this host can delete or overwrite them** in R2 before they age out.

The command is the binary's: `spawn backup dump|bundle|verify` (and `restore-drill`, by hand)
(`crates/spawn-cli/src/backup.rs`). This directory holds what it does not:
the timers, the bucket's lifecycle and lock rules, the old shell tools
(`pg-backup.sh`, `pg-restore-drill.sh`, `lib.sh`) kept for manual use, and
the drill's test.

| What | Where |
| --- | --- |
| Bucket | `spawn-store`, account q1293822641@gmail.com, location APAC, Standard class, private (no r2.dev URL, no custom domain) |
| Format | dumps: `pg_dump -Fc` (compression `SPAWN_BACKUP_DUMP_COMPRESS`, default `zstd:9`) and `gpg --symmetric` AES-256; bundles: a tar (zstd) of `manifest.json`, `repos/*.bundle` and `transcripts/`, the same `gpg` passphrase |
| Schedule | `spawn-backup.timer` (hourly, `spawn backup dump`), `spawn-backup-bundle.timer` (daily 04:00, `spawn backup bundle`), `spawn-backup-verify.timer` (daily 05:00, `spawn backup verify`), `spawn-backup-restore-drill.timer` (daily 05:30, `spawn backup restore-drill`, objective 149; installed and enabled only after 149b passes and lands, below), `spawn-backup-restore-drill-missions.timer` (daily 05:45, `spawn backup restore-drill --only missions`, objective 186: the mission repositories alone, below); `spawn-backup-restore-drill-transcripts.timer` (daily 06:15, `spawn backup restore-drill --only transcripts`, objective 200: the transcript archive alone, judged by its record, below), `spawn-backup-restore-drill-sync.timer` (daily 06:30, `spawn backup restore-drill --only sync`, objective 217: the fleet's notes folder alone, installed disabled, below); all `Persistent=true`, so a run missed while the host was off happens at next boot |
| Records | every run writes a `backup.dumped`, `backup.bundled` or `backup.verified` control record (owner-only `spawn_backup_record`); the tiers, the bundle's base and the plausibility check are read from them, never from a state file |
| Settings | `SPAWN_BACKUP_KEY_FILE`, `SPAWN_R2_ENV_FILE`, `SPAWN_BACKUP_R2_LIMIT_MB` (10000), `SPAWN_BACKUP_R2_ALERT_MB` (8000), `SPAWN_BACKUP_TRANSCRIPT_DAYS` (14), `SPAWN_BACKUP_DRILL_DATABASE_URL` (the drill's disposable server), `SPAWN_BACKUP_PREFIX` (empty in the fleet; a test run's `locktest/`) |

## Layout, locks and expiry

Object keys are `postgres/<tier>/spawn-YYYYMMDD-HHMMSS.dump.gpg` and
`bundles/<full|incr>/spawn-bundle-YYYYMMDD-HHMMSS-mmm-<8 hex>.tar.gpg`, timestamps in
UTC. A bundle's name adds its millisecond and a random part (objective 167),
so two bundles in one second never share a name, which the lock would refuse;
names still sort as the bundles ran. Retention is entirely R2's: nothing here ever deletes or overwrites.

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

| File (mode 600; Keep `/run/keep/*` 0400/0600) | Holds | If lost |
| --- | --- | --- |
| `~/.config/spawn/backup.key` (or Keep `$CREDENTIALS_DIRECTORY/backup.key.k1` / `backup.encryption-key`) | the encryption passphrase (32 random bytes, base64) | **every dump and bundle in R2 is unreadable.** Copy it off this host now (password manager or paper) |
| `~/.config/spawn/r2.env` (or Keep `$CREDENTIALS_DIRECTORY/r2.credentials`) | `SPAWN_R2_ENDPOINT`, `SPAWN_R2_BUCKET`, `SPAWN_R2_ACCESS_KEY_ID`, `SPAWN_R2_SECRET_ACCESS_KEY` | make a new token (below) |
| `~/.local/share/spawn-rsi/spawn.env` (or Keep `$CREDENTIALS_DIRECTORY/spawn.database-url` & `spawn.backup-drill-database-url`) | `SPAWN_DATABASE_URL` (live) and `SPAWN_BACKUP_DRILL_DATABASE_URL` (the fleet test database's server; the drill makes and drops its own database there) | — |

Both `spawn backup` / `spawn store` and `deploy/backup/lib.sh` support Keep Wave K3
(`LoadCredential=` into `$CREDENTIALS_DIRECTORY`) with fail-closed validation
(symlinks, empty files, or group/world permissions exit `78` / `EX_CONFIG` without
falling back to legacy `.env` files), while falling back smoothly to `~/.config/spawn/*`
and `spawn.env` when Keep credentials are not yet delivered to a host.

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
hold, and a restore of it makes an empty repository. Every repo entry carries
its `file` in the archive, `null` when the archive holds none of it, and the
record and the manifest use the same shape.

Every repo entry of an increment carries a `delta` (`added`, `moved`,
`removed`, each the ref names; their commits are in `heads` where they still
exist) and a `bundled` list naming the refs the bundle actually holds
(empty when nothing needed bytes). An increment whose refs all point into its
base — a tag added at a commit the base already holds, a branch moved back to
one, a branch removed — would write no bytes, and git refuses to make an
empty bundle; the run says so itself: no file, `bytes` 0, the `delta` and
`bundled` `[]`. A reader can tell a source that moved in a way that needs no
bytes from a bundle that failed.

`spawn backup verify` is the drill (below): the newest good dump restores to
its record, and the newest bundle is opened and compared with its own. Exit
0 both pass; 1 either does not (missing, corrupt, a sha256 or a count that
differs, a failed restore, a bundle that does not open or verify); 2 cannot
run (no drill server named, no credentials, no key).

## Install

The oldest curl it works with is 7.88 (Debian 12's): every request names
its payload hash (`x-amz-content-sha256: UNSIGNED-PAYLOAD`), which 7.88 does
not send itself and R2 requires, and every listing sends its query sorted
by name, as SigV4 signs it (7.88 signs the query as written).

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

The restore drill's timer (objective 149) is installed and enabled only
after objective 149b passes and lands; until then the drill runs only by
hand, attended, by the lead. Until 149b the drill removes its scratch
directory by name after a check (`remove_dir_all`), which is no removal to
run unattended every day; 149b removes it through the fd it was made with.
By hand, with the bundle's environment (so its records reach the bundle's
store):

```sh
(set -a; . ~/.local/share/spawn-rsi/spawn.env; set +a
 SPAWN_WORKSPACE_ROOT=~/.local/share/spawn-rsi/ws ~/.local/share/spawn-rsi/bin/spawn backup restore-drill)
```

Once 149b has passed and landed, and is deployed:

```sh
install -m 644 deploy/systemd/spawn-backup-restore-drill.service \
  deploy/systemd/spawn-backup-restore-drill.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-backup-restore-drill.timer
```

The mission repositories' drill (objective 186) runs on its own, daily at
05:45: `spawn backup restore-drill --only missions` unpacks from each bundle
only `manifest.json` and `repos/*`, never a transcript, so its scratch
directory holds no transcript and is removed whole; it records
`backup.drilled.missions` alone. It reads R2 with the bundle's own
credentials (`SPAWN_R2_ENV_FILE`), GET only: no read-only token is on this
host (that one is the Mac's, for a restore on a new machine). Once 186 has
landed and is deployed:

```sh
install -m 644 deploy/systemd/spawn-backup-restore-drill-missions.service \
  deploy/systemd/spawn-backup-restore-drill-missions.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-backup-restore-drill-missions.timer
```

The controllers' drill (objective 198) runs daily at 06:00: `spawn backup
restore-drill --only controllers`. For each controller an active mission's
gate names (`$SPAWN_LIVE/controllers/<name>/...`), current and previous, it
checks, read only, that the live directory equals `git archive` of its
`.controller` commit from the live source (left out: `.controller`, `.deps`,
`web/node_modules`, which deploy/controller.sh adds), and (objective 199)
that X, the newest commit of that history the newest bundle chain holds,
restored and extracted from it equals X from the live source; the lag from
X to the live commit (commits, hours) is recorded, never a failure (deploys
are many a day, bundles one). One
`backup.drilled.controllers` per controller; the run exits 0 only when every
one passed, so the recovery table's row for the controllers names the unit:
`{"name": "controllers", "unit": "spawn-backup-restore-drill-controllers.service"}`
(an owner config change, after landing). Installed disabled; enabled after a
supervised first run:

```sh
install -m 644 deploy/systemd/spawn-backup-restore-drill-controllers.service \
  deploy/systemd/spawn-backup-restore-drill-controllers.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user start spawn-backup-restore-drill-controllers.service   # supervised; then:
systemctl --user enable --now spawn-backup-restore-drill-controllers.timer
```

The transcript archive's drill (objective 200, restore drill 3) runs on its
own, daily at 06:15: `spawn backup restore-drill --only transcripts` unpacks
from each bundle of the newest chain only `manifest.json` and
`transcripts/*` (decrypted in a pipe, as the missions drill does), restores
the chain's slices into its own folder, and judges each file by the newest
record alone: the size the record names and the record's
`transcripts.sha256` (a record bundled before objective 149 names none and
fails it). It never reads live: the archive, `~/.claude`, the session
backups and every path outside its folder stay unopened. The restored files
are copies; the folder (under `/data/tmp`, never under `$HOME`,
`$SPAWN_LIVE` or the workspace root) is removed by its literal path once it
is checked to be the one the drill made and to hold nothing but directories
and regular files; anything else (a symlink, a device) fails the drill and
leaves the folder for a person. Before it fetches anything it refuses, with
one line and exit 2, when the chain's ciphertext, slices and restored files
would leave less than 40 GB free on the folder's file system
(`SPAWN_BACKUP_DRILL_FLOOR_MB`, default 40960). It records
`backup.drilled.transcripts` alone and prints counts, never a transcript's
content. Exit 0 passed, 1 failed, 2 cannot run. Installed disabled; enabled
after a supervised first run (the owner or the lead), once 200 has landed
and is deployed:

```sh
install -m 644 deploy/systemd/spawn-backup-restore-drill-transcripts.service \
  deploy/systemd/spawn-backup-restore-drill-transcripts.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user start spawn-backup-restore-drill-transcripts.service   # supervised; then:
systemctl --user enable --now spawn-backup-restore-drill-transcripts.timer
```

The unit's result is the run's, so guardrail U1's recovery table (the Core
lines file's `recoverability.drills`, an owner config change made after
landing) names the unit: `{"name": "transcripts", "unit":
"spawn-backup-restore-drill-transcripts.service"}`: passed when its last
run exited 0.

The notes folder's drill (objective 217) runs on its own, daily at 06:30:
`spawn backup restore-drill --only sync` unpacks from each bundle of the
newest chain only `manifest.json`, `sync/manifest.json` and `sync/files/*`
(decrypted in a pipe, as the transcripts drill does). Each link's notes
manifest must be the sha256 its record names (`sync.manifest_sha256`), and
every file of the newest manifest is restored from the newest link that
took it, with the newest manifest's size and sha256; a file no link holds,
other bytes, or a newest record whose notes part is missing or was dropped
(`refused`) is a backup that cannot be restored. Then the folder the record
names (`sync.dir`, `/var/tmp/spawn/sync`) is compared with it, read only:
the same bytes; changed since (other bytes, written at or after the
bundle's start), removed since and made since (written at or after the
bundle's start) are expected; a file listed as skipped or changed while bundled is
said apart; anything else, such as other bytes last written before the
bundle started (the backup holds older bytes than the folder had then) or
an older file the backup lacks, is a mismatch. Nothing is written to, created
in or removed from the notes folder. The drill's folder (under `/data/tmp`,
never under `$HOME`, `$SPAWN_LIVE`, the workspace root or the notes folder)
holds only directories and regular files and is removed as the transcripts
drill's; the floor counts the chain's ciphertext and notes files. It
records `backup.drilled.sync` alone and prints counts and paths, never a
file's content. Exit 0 passed, 1 failed, 2 cannot run. The timer ships
disabled: the lead enables it after one supervised run on live, and adds
the recovery table's row then (not part of 217):

```sh
install -m 644 deploy/systemd/spawn-backup-restore-drill-sync.service \
  deploy/systemd/spawn-backup-restore-drill-sync.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user start spawn-backup-restore-drill-sync.service   # supervised; then:
systemctl --user enable --now spawn-backup-restore-drill-sync.timer
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
names and nothing it does not — a full bundle holds every head; an increment
holds a subset, which its manifest names as `bundled` (`--not` leaves out a
ref that did not move, and one whose new commit the base already has), and
the bundle must list exactly those — and a full bundle must additionally
pass `git bundle verify` in an empty repository (an increment's
prerequisites are its base's commits, which the drill does not fetch). A store with no bundle yet says so in the record and passes. The result is the record's `bundle`
object; a bundle that fails makes the day's verdict a failure.

`spawn backup restore-drill [--into <dir>] [--keep] [--only missions|controllers|transcripts]` (core (ii); by hand,
attended, by the lead; daily from `spawn-backup-restore-drill.timer` at 05:30
once 149b has passed and landed, see above)
restores the sources and the transcript archive themselves, not only opens
them. It takes the newest stored bundle and the chain of increments back to
its full bundle (each record's `base_event_id`; a base that stored nothing
breaks the chain and fails the drill), fetches and decrypts each (the
object's `x-amz-meta-sha256` and the archive's SHA-256 against the record's),
and applies them in order into a new directory of its own, mode 700, under
`--into` (default `/data/tmp`; never under `$HOME`, `$SPAWN_LIVE` or the
workspace root, symlinks resolved):

- the transcript archive: each slice at its offset (whole at 0, an append at
  exactly the size restored so far), each file then the size the newest
  record names, then each file's SHA-256 against the one the newest record
  names (`transcripts.sha256`, recorded by `spawn backup bundle` since
  objective 149: the bytes as bundled), so the backup is judged without
  live; a record bundled before 149 names none, and the drill says so; then
  compared with `<root>/.transcripts` by size and SHA-256. A
  live file that grew since the bundle (its first bytes the restored file)
  and one made since are expected; one last written before the bundle's
  window (`SPAWN_BACKUP_TRANSCRIPT_DAYS`) is in no bundle (so it has no
  off-host copy once the bundles that held it expire: an owner question,
  sync/recovery.md), counted apart and
  not a failure; a live file changed, shorter, gone, or written within the
  window and not restored is a mismatch;
- every mission source: `git clone --mirror` of its full bundle, each
  increment's bundle fetched on top, the refs set to the newest record's
  heads, `git fsck --connectivity-only`; compared with the live repository's
  `refs/heads` and `refs/tags`. A ref live moved on to a descendant, added or
  removed since is expected; one live moved to a commit that does not descend
  from the restored one is a mismatch.

One line per resource, then `passed` (exit 0) or `FAILED` (exit 1: a
mismatch older than the bundle, or a backup that cannot be restored); 2 when
it cannot run (no settings, a refused `--into`, no stored bundle). It reads
live and writes none of it, records its verdict per resource
(`backup.drilled.transcripts`, `backup.drilled.missions`, objective 104;
the Core lines recovery rows read them), reads the key as `verify` does
and prints none of it, and removes its directory at the end (`--keep` leaves
it) only after checking it is still the one it made. It reads the bundle
records from the live database, as `verify` does: it drills the backup, not
a restore of a host that lost its database too. `test-restore-drill.sh`
tests it end to end: the binary's own `spawn backup bundle` against a fake R2
(a local directory behind a small HTTP server) and a throwaway database,
under a fake HOME.

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
  -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
  "$SPAWN_R2_ENDPOINT/$SPAWN_R2_BUCKET?list-type=2&prefix=postgres%2Fdaily%2F"
curl --aws-sigv4 aws:amz:auto:s3 --user "$SPAWN_R2_ACCESS_KEY_ID:$SPAWN_R2_SECRET_ACCESS_KEY" \
  -H "x-amz-content-sha256: UNSIGNED-PAYLOAD" \
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
  -o bundle.raw -d spawn-bundle-YYYYMMDD-HHMMSS-mmm-<8 hex>.tar.zst.gpg
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

Two entries name no file: one marked `empty` is a source with no branches and
no tags — there is nothing to clone, and `git init --bare <name>.git` makes
the empty repository its record describes; one carrying a `delta` and no file
is a change that needs no bytes (a tag added at a commit the base holds, a
branch removed) — restore the source as it stood at the base, and nothing
changes.

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
