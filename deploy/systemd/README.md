# systemd user units

The units this host runs, with `%h` for the home directory.

**An owner edit is live only after its deploy.** No unit runs or reads
anything from the owner's checkout (`~/projects/Spawn`): binaries and UI come
from `~/.local/share/spawn-rsi` (installed by `deploy/deploy.sh`), and the
gate, its scenarios, the objective backlog and the maintenance script from
the controller, the deployed commit's tree (below). Editing `gate.sh`, a
scenario or `rsi/backlog` in the checkout, or merging it to the checkout's
main, changes nothing live until that commit is gated and deployed.

| Unit | Runs | Stops on its own |
| --- | --- | --- |
| `spawn-loop` | `spawn loop`: Bridge, Observer, dispatcher, supervisor and resident liveness | never (`Restart=always`) |
| `spawn-integrate` | `spawn integrate`: lands gated missions' work through the controller's `scripts/rsi/gate.sh` (below), assigns objective tasks (RSI is an objective mission) | never; a halt is per mission, cleared by `spawn mission resume` |
| `spawn-serve` | `spawn serve`: UI and API, read as `spawn_reader`, and a person's input to a session typed as `spawn_human` (the one write, `POST /api/session/<id>/input`); the UI is the build `deploy.sh` installs in `~/.local/share/spawn-rsi/web` (`SPAWN_WEB_DIR`), on `SPAWN_SERVE_ADDRESS` (default `127.0.0.1:8787`). Off loopback (e.g. `0.0.0.0:8787` for the LAN) it refuses to start without `SPAWN_SERVE_TOKEN`; open the UI once as `http://<host>:8787/?token=<token>` | never; without the token it exits and restarts every 3 s |
| `spawn-jev-relay` (and `.path`) | `scripts/jev-relay.py` on 127.0.0.1:8790, holding the API key (Keep's `/run/keep/typesafe.api-key`, by `LoadCredential`; objective 201). Without the file (after a reboot, until Keep delivers again) it fails with 243 and stays stopped; `spawn-jev-relay.path` starts it when Keep writes the file | never; stopped while the key file is absent or empty, until Keep writes it |
| `spawn-model-relay` | `scripts/model-relay.py` on 127.0.0.1:8791: the engines' model calls, `deepseek-*` to DeepSeek with `DEEPSEEK_API_KEY` (from `~/.hermes/.env`, held only here), everything else to the gateway of `~/.claude/settings.json` with the engine's own auth ([fleet-architecture.md §3.3](../../docs/fleet-architecture.md#33-engine-models)) | never |
| `spawn-fleet-test-db` | PostgreSQL 16 on a Unix socket only (`~/.local/share/spawn-rsi/fleet-pg`): the fleet's test server, which engines get as `SPAWN_TEST_DATABASE_URL` once it checks out (below) | never (`Restart=on-failure`) |
| `spawn-backup` (`.timer`) | `spawn backup dump`: the hourly encrypted `pg_dump` of the event log to R2, bucket-locked, expired by R2's lifecycle; install, keys, windows and cost in [deploy/backup](../backup/README.md) | oneshot, hourly at :00 + up to 5 min |
| `spawn-backup-bundle` (`.timer`) | `spawn backup bundle`: the daily encrypted bundle of the mission sources and the 14-day transcript window — a full bundle the ISO week's first (or when none is within 7 days), an increment against its base otherwise; with `SPAWN_BACKUP_SYNC_DIR` (the unit sets `/var/tmp/spawn/sync`) the notes folder too, read only, staged on the workspace's disk (never /tmp), its new or changed files scanned for credential values first (a hit, a place the scan could not search or a file it cannot read drops that part, never the rest); a failed run retries twice (15 min apart) | oneshot, daily at 04:00 + up to 1 h |
| `spawn-backup-verify` (`.timer`) | `spawn backup verify`: the daily restore drill — the newest dump restored into a scratch database of its own on `SPAWN_BACKUP_DRILL_DATABASE_URL` and compared with its record (never the live database), and the newest bundle fetched, unpacked and compared with its own | oneshot, daily at 05:00 + up to 1 h |
| `spawn-nightly@<source>` (`.timer`) | `scripts/rsi/nightly.sh <source>` from the controller: the scenarios in `rsi/scenarios/nightly/` (moved out of the gate), each judged by the gate on live `main` against itself, alone on a held host; the fact file `$SPAWN_WORKSPACE_ROOT/nightly/<source>.json` (green, red, flaky or skipped) and one `nightly_red` alert per red (below) | oneshot, daily at 02:30 + up to 10 min |
| `spawn-nightly-stale@<source>` (`.timer`) | `scripts/rsi/nightly.sh --stale <source>`: whether the nightly covers anything (two skipped runs in a row, no fact younger than 26 h, or none though its timer fired: one `nightly_stale` alert); no run, no gate | oneshot, hourly |
| `spawn-credential-health` (`.timer`) | `scripts/keep/credential-health.sh` from the controller, every 5 minutes: Keep's `/run/keep/typesafe.api-key` is present (after a 15-minute boot grace), loaded by `spawn-jev-relay` (not newer than its start), the relay is up, its newest relayed calls are not refused (401/403) or failing (3 non-200), and no legacy `typesafe.env` or `EnvironmentFile=` is back. One owner alert per fault class (kind `credential_health`), cleared when it ends; it opens no credential | oneshot, every 5 min (first 5 min after boot) |
| `spawn-deploy-on-landing` (`.timer`) | `deploy/on-landing.sh` from the controller (#54): deploys what the fleet landed on the Spawn source, `deploy.sh --main` from its own checkout of main, once the store holds a gate-accepted landing of it, after a quiet spell (at the latest a max wait), never while the source is held, a deploy is pending or running, or the nightly is red (below) | oneshot, a minute after the last tick |
| `spawn-maintenance` (`.timer`) | `scripts/maintenance.sh` from the controller: the gate's leftover scratch (`$TMPDIR/spawn-gate.XXXXXX` whose gate is gone, by the pid, start time and boot the gate recorded in it; never a running gate's, whatever its age), deploy.sh's dumps in the backups directory beyond the newest 5, and a VACUUM (ANALYZE) of the test databases' own catalogs, the fleet's and the gates' shared spawn_test (`scripts/vacuum-catalogs.sh`, P21: refused on the live database's port or name, left alone while another session is connected, drops no schema; `--url-var SPAWN_TEST_DATABASE_URL --full`, a dry run unless `--apply`, is the one-time rewrite that gives a leak's room back, run by hand at a quiet moment, never on the timer); nothing Spawn did not create ([docs/storage.md](../../docs/storage.md#disk-room)) | oneshot, hourly |
| `spawn-session-backup` (`.timer`), `spawn-session-backup-drill` (`.timer`) | `deploy/session-backup.sh`: every account's Claude Code sessions (`~/.claude/projects`, `history.jsonl`, `file-history`) copied as a version (a reflink copy, its manifest and its file and dir counts), thinned to one version per day after 48 h but never a version that holds a file no later one holds (both units run `thin --dry-run`: print-only); daily, a restore of the newest into scratch, checked file by file. **System** units, run as root, installed by hand (below, "Session backups") | oneshot, every 15 min; the drill daily at 05:30 |

`spawn-loop` uses `KillMode=process` so a restart keeps the agents' tmux
server alive. That only works because `ExecStart` is `spawn loop` itself: with
a wrapper script as the main process, systemd would kill the wrapper and leave
the old loop running beside the new one. SIGTERM (a stop or a deploy's
restart) ends `spawn loop` after its current pass, once the branch pushes
still queued in its memory are made; `TimeoutStopSec` leaves room for that.
A loop killed outright loses none either: the next one publishes again the
branch of every agent with an open task.

Install: copy to `~/.config/systemd/user/`, then
`systemctl --user daemon-reload && systemctl --user enable --now spawn-loop spawn-integrate spawn-serve spawn-jev-relay spawn-jev-relay.path spawn-model-relay`.

The relays' scripts run from `~/.local/share/spawn-rsi/bin/`, installed by
hand the first time; after that `deploy.sh` installs a changed relay script and try-restarts its unit (a unit that is not installed is skipped). The model relay:

```sh
install -m 755 scripts/model-relay.py ~/.local/share/spawn-rsi/bin/
python3 -I scripts/model-relay-test.py        # self-test, loopback only
cp deploy/systemd/spawn-model-relay.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-model-relay
curl -s http://127.0.0.1:8791/healthz         # {"ok":true}
journalctl --user -u spawn-model-relay -n 5   # the upstream hosts it routes to
```

Then point the loop at it: `SPAWN_MODEL_RELAY=http://127.0.0.1:8791` in
`spawn.env`, and restart `spawn-loop` (agents keep running,
`KillMode=process`). Unset or `off`, engines run on Claude Code's own
settings and cheap tasks fail. The loop checks the relay before every Claude
start: while it is down, default engines start without it (a tick warning)
and cheap starts fail; engines already running keep the settings they
started with. `SPAWN_CHEAP_MODEL` (default `deepseek-flash`) and
`SPAWN_SMALL_MODEL` (default `deepseek-flash`) go in `spawn.env` too. The relay's gateway is `MODEL_RELAY_UPSTREAM`, else the
`env.ANTHROPIC_BASE_URL` of `~/.claude/settings.json` read at start.
Configuration lives in `~/.local/share/spawn-rsi/spawn.env` (KEY=VALUE),
including `SPAWN_SUPERVISOR=1`, `SPAWN_MISSION_FILE`, and `SPAWN_SERVE_ADDRESS`
with `SPAWN_SERVE_TOKEN` when the UI serves the LAN (a URL-safe secret, e.g.
`openssl rand -hex 32`). `SPAWN_SERVE_OPEN_NETWORKS` (e.g. `192.168.31.0/24`)
lets the owner's intranet read without the token, by the server's address
(`http://192.168.31.112:8787/`); a client elsewhere (Tailscale, a container)
still needs it.

## Session backups

`deploy/session-backup.sh` copies the Claude Code sessions as versions into
`/var/backups/spawn-sessions`. Its header has the rules: what a version is,
thinning (never a version holding a file that no later version holds; the
first thinning of a root only prints), restore (into scratch only) and the
drill. Its test runs under a scratch root and HOME only:
`bash deploy/test-session-backup.sh`.

The units are **system** units, in `deploy/systemd/system/`. The versions
are owned by `spawn-backup`, a system user with no login, no home and no
supplementary group. The root directory is 0711, each account's directory
0700, and `status.json` 0644. Root only starts each account, which reads its
own sessions, and `spawn-backup`, which writes its own tree, and joins their
pipes. Both units run `thin --dry-run` until the owner allows removal.

Since objective 147, every new version is also written as blocks beside its
directory (`deploy/session-blocks.py`, stage 1 of
sync/design-143-session-block-store.md), and copy, thin, restore and the drill
take a lock on the root (the script's header says how). The directory stays
the version; nothing is converted or removed.

**Nothing here is live until it is installed by hand, with sudo (the lead's
only).** The units run `/usr/local/lib/spawn/session-backup.sh`, a root-owned
copy, which runs `session-blocks.py` from beside itself, and `deploy.sh`
installs neither the scripts nor the units. These are the reviewed files of
this tree. Until the lead installs 147, the host runs 146's
`session-backup.sh` (a2437d463f2769bfff4f2da9065dde20ed6570b3218bee40f87e74774555ca85,
installed 2026-11-12 from 4f247ef1) and has no `session-blocks.py`:

```text
e9e5f2431e1e8289972fe969eee7a386d8e5fb5fd4c82827a7eebdca0c898589  deploy/session-backup.sh
c534ff6dbecfdb32afce70a337b39266f6b92b534db070a19c6bcae37364f57b  deploy/session-blocks.py
d9befa41fdb9743f6eebfc257bf3b26ea54a24dc95b0af90fe40d319f8695a14  deploy/test-session-backup.sh
e653439c64c7bc9cdea7c5e1fe4e2c3f15fa9fd73e97b3ee572c4224c2ef4881  deploy/systemd/system/spawn-session-backup-drill.service
51be45c296935f0356322d298d65084138f644826fcea78ddaf6d4eadebb310c  deploy/systemd/system/spawn-session-backup-drill.timer
c7bccae226da53c5a7ec8c3597bdb71a35b7d8eb3f70fb3ebc1bf10ef1b02148  deploy/systemd/system/spawn-session-backup.service
229ab22eb5e4604125e4a677cf1d5a52f67912de63a8578414e6d0d3d4c0dccb  deploy/systemd/system/spawn-session-backup.timer
```

A later change installs the same way. Root never runs or installs a file an
account can change, and runs no git in a checkout. Root copies the files
into a new root-owned directory, checks each sha256 against the reviewed
commit's, and only then installs: `session-backup.sh` with `install -o root
-g root -m 755` and `session-blocks.py` with `-m 644`, both to
`/usr/local/lib/spawn/`; the units (when they change) with `-m 644` to
`/etc/systemd/system/`, then `systemctl daemon-reload`. Before the install,
the test runs as root from that copy, in the pilot's form
(`SPAWN_TEST_ACCOUNT`, `SPAWN_TEST_BACKUP_USER`).

`spawn serve` reads the status file the units write: `SPAWN_SESSION_BACKUP_STATUS`
for the storage figure, and `recoverability.session_backup_status` for the
core lines' backup age.

Restore is never onto the live sessions. It goes into scratch, then is
copied by hand. `spawn-backup` archives the version, and the account
extracts it into a directory it made and hashes what it wrote.

```sh
sudo SPAWN_SESSION_BACKUP_ROOT=/var/backups/spawn-sessions SPAWN_SESSION_HOMES=/home/ckl \
  SPAWN_SESSION_BACKUP_USER=spawn-backup bash /usr/local/lib/spawn/session-backup.sh list ckl
sudo SPAWN_SESSION_BACKUP_ROOT=/var/backups/spawn-sessions SPAWN_SESSION_HOMES=/home/ckl \
  SPAWN_SESSION_BACKUP_USER=spawn-backup bash /usr/local/lib/spawn/session-backup.sh restore ckl <version> /data/tmp/restore-<date>
```

## Fleet test database and credentials

Workers run tests that need PostgreSQL. They get a server of their own,
never the gates' shared one (the system cluster `16-test` that the loop's
and the integrator's `SPAWN_TEST_DATABASE_URL` names) and never the live
one: a worker's test load there failed fleet gates, and a worker without a
test database went looking for one in `spawn.env` (2026-11-08).

`spawn-test` is a system cluster (`postgresql@16-test`, run by `postgres`,
created with root). The fleet's is a user cluster instead, like the relays:
no root, created by a script in the checkout, kept running by a user unit
(the user lingers, so it starts at boot), and `spawn-loop` wants it and
starts after it. Its data is test data, `fsync` off; lost, it is made again.
Engines log in as `spawn_fleet`, not a superuser: a superuser login would
turn `COPY ... PROGRAM` and `pg_read_file` into a way around every deny
rule an engine has. Spawn's spawn-postgres tests pass on it, 144 of 144
(the script's header; the two checks that need a superuser skip with a
reason).

What protects the credentials ships in the binary: the deploy, then the
units, in this order.

1. Deploy the commit (`deploy/deploy.sh`, above). The new binary gives
   engines the deny rules (`CREDENTIAL_DENY` in
   `crates/spawn-core/src/bridge.rs`: Claude Code rules in bypass mode,
   not a sandbox, [design.md](../../docs/design.md)), makes `spawn loop`,
   `spawn integrate` and `spawn serve` not dumpable (their
   `/proc/<pid>/environ` is no longer readable by the engines, which run as
   the same user), and passes the fleet test URL only once it checks out.
   This deploy's own base-cache warm-up finds no gate environment file yet
   (step 2) and says "not warmed"; the next deploy warms it.
2. Install the units: `spawn-integrate` now writes its gate environment to
   `~/.config/spawn/gate-env` (0600; `SPAWN_GATE_ENV_FILE` in its unit),
   which `deploy.sh` reads instead of the integrator's environ.

```sh
sh deploy/fleet-test-db.sh       # once: initdb, logins spawn_fleet_admin and spawn_fleet, database spawn_fleet_test
printf 'SPAWN_FLEET_TEST_DATABASE_URL=%s\n' "$(cat ~/.config/spawn/fleet-test-database.url)" \
    >> ~/.local/share/spawn-rsi/spawn.env
cp deploy/systemd/spawn-fleet-test-db.service deploy/systemd/spawn-loop.service \
    deploy/systemd/spawn-integrate.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-fleet-test-db
systemctl --user restart spawn-loop spawn-integrate   # agents keep running (KillMode=process)
```

3. **Running engines keep the settings and environment they started with.**
   The deny rules and the test URL reach an engine only at its next start
   or resume: an engine that was running before the deploy has neither
   until then. Stop and resume the idle workers after the restart to close
   that window; a busy one gets them at its next resume.

The loop gives an engine that URL as `SPAWN_TEST_DATABASE_URL` (the pane's
private environment file, never a command line), and the task prompt says
so. It fails closed: the URL is given only once the fleet's server and
every other one configured (the gates' `SPAWN_TEST_DATABASE_URL`, the live
`SPAWN_DATABASE_URL`) answer with their `system_identifier`, and the
fleet's differs from each. Until then each launch goes without it, the
loop's log says why once, and the next launch checks again (a fleet server
that comes up late is given from then on). Without
`SPAWN_FLEET_TEST_DATABASE_URL`, engines get no test database at all.

## Disk room

Three guards keep a full disk from corrupting state or wedging the fleet
([docs/storage.md](../../docs/storage.md#disk-room)); the first two ship in
the binary and the checkout, the third is a timer to install.

- The gate exits 3 (an abort, retried) with less than
  `SPAWN_GATE_MIN_FREE_GB` (default 10) GiB free in `TMPDIR`.
- `spawn loop` records a `host.disk` warning when the workspace root or
  `TMPDIR` falls under `SPAWN_DISK_WARN_GB` (default 20) GiB free, and starts
  no new executions under `SPAWN_DISK_FLOOR_GB` (default 2). The fleet view's
  health strip shows free space now.
- `spawn-maintenance.timer`, installed by hand. Look first at what it would
  remove (it deletes nothing in a dry run):

```sh
set -a; . ~/.local/share/spawn-rsi/spawn.env; set +a
SPAWN_MAINTENANCE_DRY_RUN=1 sh ~/.local/share/spawn-rsi/controllers/spawn/scripts/maintenance.sh
sh scripts/test-maintenance.sh                    # in a checkout; throwaway directories only
bash scripts/test-vacuum-catalogs.sh              # in a checkout; a throwaway cluster of its own, a fake HOME
cp deploy/systemd/spawn-maintenance.service deploy/systemd/spawn-maintenance.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-maintenance.timer
systemctl --user start spawn-maintenance.service  # one run now, optional
journalctl --user -u spawn-maintenance -n 20
```

Settings, in `spawn.env`: `SPAWN_MAINTENANCE_KEEP_DUMPS` (5),
`SPAWN_MAINTENANCE_SCRATCH_AGE_MIN` (1440: only for a scratch that holds no
record of its gate, and never under `SPAWN_GATE_WALL_CLOCK_MINUTES`).

- `spawn-nightly@spawn.timer` (gate v2 step 5a, sync/design-nightly-min.md),
  installed by hand. It takes every host scenario slot as the run holding
  it ends (up to 2 h, else the fact file says `skipped`), so the host
  drains instead of having to fall quiet by itself, then lets slot 1 go to
  its own gate and holds 2..N for its whole run (a gate that needs one
  meanwhile waits, and past its `SPAWN_GATE_SLOT_WAIT_SECS` has no room:
  judged again after the backoff),
  and judges each `rsi/scenarios/nightly/*.sh` with the gate: main against
  itself, the scenario as a check (run again on a failure; a pass the second
  time is `flaky`), with the integrator's gate environment
  (`SPAWN_GATE_ENV_FILE`). A red run fires one `nightly_red` alert (`spawn
  alerts`, the home page), keyed by the red's first main, naming the failed
  scenarios and the landings since the last green; the first green or flaky
  run clears it. The gate's log of each scenario's last run is kept at
  `$SPAWN_WORKSPACE_ROOT/nightly/<source>.<scenario>.log`. While a red
  lasts (the fact's `red_since`, kept through a skipped run), the
  integrator judges no task of that source, so none lands ("held: main of
  <source> is red since <sha> on the nightly"); the owner's pushes still
  land, and the first green or flaky run, the timer's or
  `nightly.sh <source>` by hand after the fix, releases the tasks (step
  5b). A run stopped by its unit's timeout (6 h)
  writes `skipped`, saying so. Two skipped runs in a row, or no fact younger
  than 26 h, fire one `nightly_stale` alert (from the run itself, and hourly
  from `spawn-nightly-stale@spawn.timer`, so a nightly that never runs is
  told too); the first run that judges clears it.

```sh
SPAWN_TEST_DATABASE_URL=… SPAWN_BIN=target/debug/spawn bash scripts/rsi/test-nightly.sh   # in a checkout
cp deploy/systemd/spawn-nightly@.service deploy/systemd/spawn-nightly@.timer \
  deploy/systemd/spawn-nightly-stale@.service deploy/systemd/spawn-nightly-stale@.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now spawn-nightly@spawn.timer spawn-nightly-stale@spawn.timer
systemctl --user start spawn-nightly@spawn.service   # one run now, optional (after a fix: clears a red)
jq . ~/.local/share/spawn-rsi/ws/nightly/spawn.json
journalctl --user -u spawn-nightly@spawn -n 20
```

- `spawn-deploy-on-landing.timer` (#54), installed by hand. A landing the
  integrator makes on the Spawn source (`<live>/missions/spawn.git`, whose
  main is the live system) is deployed without a hand step. Only that
  source: a landing on any other (Supply PA's) deploys nothing. Each tick
  (deploy/on-landing.sh, which says why it waits, once per change) deploys
  the missions main when the store holds a gate-accepted landing of exactly
  that commit (`spawn mission landing`: a task.landed of a task of a
  mission on the source, or an owner's gated source.moved; never a file)
  and no landing came for `SPAWN_DEPLOY_QUIET_SECS` (300), or
  `SPAWN_DEPLOY_MAX_WAIT_SECS` (1800) after the first landing not live yet.
  It waits while the source is held (an owner gate, or another deploy),
  while `deploy.pending` exists, while another deploy holds the lock, and
  while the nightly is red (`red_since` in the fact: a red main is a known
  regression; deployed at the first tick after a green night, or by
  hand). It holds the source while it deploys, so no landing moves main
  under the build (reason `deploy-on-landing <main>`, at most
  `SPAWN_DEPLOY_HOLD_SECS`, 3600); a tick or deploy killed outright leaves
  that hold, and the next tick releases it. It deploys with main's own
  `deploy/deploy.sh --main` from `<live>/deploy-checkout` (a clone it makes;
  the UI's dependencies installed with pnpm when the lockfile changed), as
  the owner deploys: the drain, the integrator's restart and the gates that
  outlive it are deploy.sh's. Every deploy, the owner's too, records how it
  ended (`deploy.finished`); a failure raises one `deploy_failed` alert,
  which the next successful deploy clears, and a failed main is tried again
  after `SPAWN_DEPLOY_RETRY_SECS` (1800) or at the next landing.
  `SPAWN_DEPLOY_ON_LANDING=0` in `spawn.env` pauses it. The base cache's
  warm-up that a deploy starts runs in a unit of its own
  (`spawn-warm-base-<commit>-<pid>`, deploy/detach.sh): the tick's unit is
  a oneshot, and a child left in its cgroup would be stopped with it.

  `<live>/deploy-checkout` is a build input, like the owner's checkout:
  deploy.sh exports its HEAD and links its `web/node_modules` into the
  build. Agents run as the owner's uid, so they can write it, as they can
  write the owner's checkout (the same-uid limit, docs/fleet-architecture.md
  §10.2); the build runs in the sandbox (no network, `$HOME` hidden), and
  the tick checks the tree out with `--force` at main before each deploy.

  Its unit's environment is `spawn.env` and the unit's PATH, not the
  owner's shell: start it once by hand, with `SPAWN_DEPLOY_ON_LANDING=0`
  set in `spawn.env` (it then reads the store and says it is paused), and
  read its journal before enabling the timer.

```sh
SPAWN_BIN=target/debug/spawn bash deploy/test-on-landing.sh   # in a checkout; a throwaway database
cp deploy/systemd/spawn-deploy-on-landing.service deploy/systemd/spawn-deploy-on-landing.timer ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user start spawn-deploy-on-landing.service   # once by hand, paused (above); then:
journalctl --user -u spawn-deploy-on-landing -n 20
systemctl --user enable --now spawn-deploy-on-landing.timer
```

To retire a source's nightly, stop its timers and remove its fact file
with them: a red fact left behind holds that source's tasks for good (step
5b reads it whether or not a nightly still runs).

```sh
systemctl --user disable --now spawn-nightly@spawn.timer spawn-nightly-stale@spawn.timer
systemctl --user stop spawn-nightly@spawn.service   # a run in progress would write the fact again
rm -f ~/.local/share/spawn-rsi/ws/nightly/spawn.json
```

Settings, in `spawn.env`: `SPAWN_NIGHTLY_WAIT_SECS` (7200),
`SPAWN_NIGHTLY_POLL_SECS` (60, only without a slot budget),
`SPAWN_NIGHTLY_SAMPLE_SECS` (15), `SPAWN_NIGHTLY_STALE_SECS` (93600),
`SPAWN_NIGHTLY_STALE_RUNS` (2), `SPAWN_NIGHTLY_LOCK_WAIT_SECS` (120: how long a run waits for an hourly check to let go of the lock).

## Deploying

**Hold, then gate, then deploy.** Before the owner gate, run
`deploy/hold.sh [<candidate>]` (default: the checkout's HEAD) instead of
stopping `spawn-integrate`. It records the owner's hold on the missions repo
(`spawn mission hold --source ~/.local/share/spawn-rsi/missions/spawn.git
--reason "owner gate <candidate>"`): no task lands on Spawn's `main` while the
gate judges against it, so the verdict's base stays the live `main` and the
deploy accepts it (on 2026-11-06 at 21:45 a landing during the owner's gate
made the verdict stale). `spawn-integrate` keeps running: every mission's
gates go on (a Spawn task the gate accepts waits to land), objective
assignment goes on (stopping the integrator held Supply PA's new objectives
for the whole deploy), and so do the other sources. The script prints the
hold, the `main` it was taken at and the gate command over that `main`. Then
run the gate and `deploy/deploy.sh <verdict-file>`, which releases the hold on
every exit path once it holds the deploy lock: after a success, once `main`
moved forward and `spawn mission moved` recorded it (the tasks that waited
land at the next pass, judged again on the new `main`), and after any failure.
A hold nobody releases (a deploy given up, a crash) expires by itself after
`SPAWN_SOURCE_HOLD_SECS` (3 h by default) and the supervisor is told;
`spawn mission release --source <that repo>` ends it at once, and
`spawn mission list` shows it (`holds`). The deploy itself still stops the
integrator for its few seconds of install. The first deploy of the hold
itself has no hold to take (the live binary predates it): stop
`spawn-integrate` for that gate as before.

`deploy/deploy.sh <verdict-file>` deploys the checked-out commit, and only one the gate accepted: the verdict file must be the gate's, accepted, with `"candidate"` = HEAD and `"base"` = the missions repo's current `main` (the script's header shows the gate command). Then: backup, release build (no kache) and UI build from a `git archive` export of HEAD (nothing untracked in the checkout is built) inside the bubblewrap sandbox of `scripts/sandbox.sh` (no network, `$HOME` hidden; the target directory `~/.cache/spawn/deploy-target` persists, so builds stay incremental), then with the integrator stopped it migrates the schema with the staged binary and only then swaps binary and UI (`spawn.prev` and `web.prev` kept; a deploy run again with the same build leaves them alone), switches the controller to HEAD's tree (below), moves the missions repo's `main` forward (never forced) and records `spawn mission moved`, so gated missions take the new main as their baseline instead of halting as if someone had pushed outside the gate. Between the push and that record it keeps `deploy.pending` in the live directory; a deploy that dies there is completed on its exit or by the next deploy. It takes `deploy.lock` in the live directory, so two deploys never run at once. It refuses a dirty tree, a HEAD that tracks files under a `node_modules/`, a HEAD that does not contain the current `main`, or less than `SPAWN_DEPLOY_MIN_FREE_GB` (default 10) free in `$TMPDIR` or the target directory, and restarts the services if anything fails after it stopped the integrator. `deploy/test-deploy.sh` runs it end to end against a fake live directory. It releases the owner's hold on the missions repo on every exit path (above). With `SPAWN_DEPLOY_MIRROR` set (a URL or a remote of the missions repo, in the environment or in `spawn.env`; unset, nothing is published), a deploy that succeeded then publishes the `main` it deployed there, last of all, after the restart and the hold's release: a plain `git push <mirror> <head>:refs/heads/main` from the missions repo, never forced, so a mirror whose `main` HEAD does not contain refuses it and the deploy names both commits. That, a mirror out of reach or without a credential, and a push not done within `SPAWN_DEPLOY_MIRROR_SECS` (default and at most 60) are one line on stderr and never the deploy's failure; the script holds no credential, the push authenticates as this user's `git push` does and never prompts, and pushes `main` alone (`--no-follow-tags`). The credential goes in a credential helper (`gh auth setup-git`) or a deploy key over ssh, never in the URL: git passes a URL to its helpers as an argument, readable in `/proc` by every process on the host, so a mirror that carries one (an http(s) URL with any user part, any URL with `user:password`, a remote or a `url.*.insteadOf` rewrite that resolves to one) is refused before anything is done, without the URL being printed or run; an empty `SPAWN_DEPLOY_MIRROR` in the environment deploys without the mirror. Rolling back to `spawn.prev` goes with `deploy/controller.sh switch spawn <the previous commit>` (`controllers/spawn.prev` names it), and is unsafe once the new binary has written an event kind the old one lacks ([docs/upgrading.md](../../docs/upgrading.md#deploying)).

## The controller

`~/.local/share/spawn-rsi/controllers/<name>` is a symlink to
`<name>-<commit>`, that commit's `git archive`, read-only, with what the gate
needs from a controller that git does not hold (for Spawn, `web/node_modules`,
which `check-web.sh` builds the UI with: a reflink copy of the deploying
checkout's, a few MB of metadata on btrfs; refused unless it was installed from
the commit's lockfile). `deploy/controller.sh` builds one and switches the
symlink atomically (rename over it); the one it replaces stays as
`<name>.prev`, and older ones are removed once no process uses them. The
integrator resolves the symlink once per step (gate and scenarios), so a gate
runs on one commit's files even across a switch. `deploy/test-controller.sh`
tests the switch (scratch directories only).

| Controller | Commit | Read by | Moved by |
| --- | --- | --- | --- |
| `spawn` | the deployed commit (the missions repo's `main` after a deploy) | `spawn-integrate` (`SPAWN_GATE`), `spawn-maintenance`, mission 4's `--gate .../rsi/scenarios` and `--backlog .../rsi/backlog` | `deploy/deploy.sh`, with the install |
| `supply-pa` | the live `main` of `missions/supply-pa.git` | mission 6's `--gate .../gate/scenarios` and `--backlog .../gate/backlog` (the gate script is Spawn's) | the owner's push to that repo, below |

Supply PA's tree runs on dependencies git does not hold: `pnpm install`
output from its lockfile (its manifests and lockfile are protected, so only
an owner install changes them). They live in a deps store, one tree per
lockfile hash, never in the owner's checkout, whose `.pnpm` every agent may
write:

    ~/.local/share/spawn-rsi/deps/supply-pa/<sha256 of pnpm-lock.yaml>/
    ~/.local/share/spawn-rsi/deps/supply-pa/current -> <that hash>

`deploy/controller.sh deps supply-pa <git-dir> <commit>` installs a tree
(`pnpm install --frozen-lockfile --ignore-scripts
--config.package-import-method=clone-or-copy` from the commit's lockfile,
then read-only; a hardlink into pnpm's store is refused, since making it
read-only would change the store). `--ignore-scripts` is deliberate: the
install runs as the owner with network, and a lifecycle script in the root
or a workspace package would run there. Nothing in the tree needs one
(`pnpm-workspace.yaml` sets `allowBuilds: esbuild: false`, and esbuild's
binary comes from its platform package); a dependency that does need a
build step is an owner decision and a reason to revisit the line. `update`
installs the tree when the lockfile's has none, and `build` refuses a commit
whose tree is missing. A switch points `current` at that commit's tree and
removes the trees nothing uses: not the current or the previous controller's
(`.deps` in each), not one an agent's workspace links into
(`ws/<agent>/node_modules/.pnpm`), not one a process holds. An agent's
workspace therefore keeps the dependencies it built with until the
workspace is gone.

"Read-only" is a boundary against accidents, not against agents. Engines run
as the same uid as everything else here: one that means to can hardlink a
store file (the inode is then writable by its workspace and the store
together) and write it, or chmod a file it copies. What the modes give is
the accident: a build, a test or an engine's Write tool cannot change the
store by mistake, the gate's sandbox shows it read-only, and a deliberate
change has to be a deliberate act (a hardlink, a chmod) instead of a slip.
Confining a deliberate agent takes per-agent uids or a mount per engine,
which is the owner's decision.

Engines and gates read the store through `SUPPLY_PA_DEPS`: a line
`SUPPLY_PA_DEPS=/home/ckl/.local/share/spawn-rsi/deps/supply-pa/current`
in `spawn.env` (a literal path, as systemd's EnvironmentFile does not expand
`$HOME`); Spawn hands it to every engine's pane, and the gate carries it into
its sandbox. Unset, `gate/scenarios/build`, the drivers and the default in
`gate/scenarios/bind` use that pointer anyway, and the owner's checkout is
never a source.

An owner push to a gated source other than Spawn (Supply PA): stop
`spawn-integrate`, push to `~/.local/share/spawn-rsi/missions/<repo>.git`, run
`spawn mission moved --source <that repo> --from <old> --to <new>`, then

```sh
deploy/controller.sh update supply-pa ~/.local/share/spawn-rsi/missions/supply-pa.git main
```

and start `spawn-integrate` again. Until that update, the mission keeps
judging with the scenarios and backlog of the commit before. The update
installs the new lockfile's deps tree first when it has none (it needs
network and `pnpm`), so a push that changes the lockfile is complete only
once that has run.

The move to controllers, once per host: `deploy/cutover-controller.sh
--dry-run` shows what it would do (nothing changes), then
`deploy/cutover-controller.sh` builds both controllers at the live commits,
installs these units, reconfigures missions 4 and 6 to the controller paths
(every other setting as it is) and restarts `spawn-integrate` once no gate
runs. Deploy this change first (so the integrator pins the controller and
reads tasks' scenarios from the mission's directory), then run the cutover;
it is safe to run again.

## Which objectives the gate cannot judge

The gate's sandbox is not the host: it has no systemd user manager and no
delegated cgroup (the owner's ruling, 2026-11-06), so an objective whose
scenario judges the host's resource envelopes cannot run in it. Such a
scenario exits 6, the gate's host-fault exit, with its own `cannot_judge`
fact (`rsi/scenarios/029.sh`'s `cannot_judge()`). On the base the gate
caches the result `cannot_judge`, so the scenario stops no other landing;
on the candidate it is skipped as a regression, and an objective-29 task is
never rejected or aborted for it: the integrator records a `host_fault`
with the evidence `cannot_judge`, retried after the backoff and counted
toward no halt. The owner judges that objective outside the gate (mission
7), and marks its scenario `# rsi: task-only`, so the gate runs it only as
its own task's scenario, never on the base for another gate nor as a
regression (scripts/rsi/gate.sh).

| Objective | Scenario | Why the gate cannot judge it |
| --- | --- | --- |
| 029 | `rsi/scenarios/029.sh` | every execution and gate runs in a systemd user scope with memory and CPU limits; the sandbox has no user manager to make one |

A new objective of this kind says so in its scenario's header and adds a row
here.

One fact of the same kind comes from the gate itself, not from a scenario:
`dependencies`. When a candidate's dependency graph does not resolve
offline and its base's does not either, the host's cargo registry lacks a
crate main needs (a runner never sent it, a pruned cache): the gate writes
`{"cannot_judge":true,"scenario":"dependencies","runs":"1",...}` and exits
6, and the integrator records it as above. It is any objective's, and it
ends when the host gets the crate; a candidate whose base resolves is
still rejected for its graph.
