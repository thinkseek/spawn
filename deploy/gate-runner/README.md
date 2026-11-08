# The remote gate runner

A gate run is self-contained and stateless: it exports two commits, builds
one, runs the controller's scenarios in a sandbox, and writes one JSON line.
That makes it the unit of work that can leave this machine, and the owner's
decision (2026-11-08) is to pool capacity by shipping gate runs, not to build
a scheduler. The primary keeps the engines, the workspaces and the live
database; a runner judges gates.

- `deploy/gate-runner.sh` (the primary side) packages a job, ships it over
  SSH, runs the gate there, and brings the verdict and the log back.
- `deploy/gate-runner/setup.sh` installs this side on a runner machine;
- `deploy/gate-runner/remote.sh` is the helper it installs
  (`<root>/bin/remote`), and the protocol the dispatcher speaks;
- `deploy/test-gate-runner.sh` runs the whole thing against a fake runner (a
  local directory and an `ssh` stub), so the failure paths exist before any
  machine does.

Nothing here changes the gate's verdict semantics: the remote gate is the
same `scripts/rsi/gate.sh` shipped in the job, judged by the same exit
statuses and the same verdict file. Nothing about the live fleet changes
until the PoC is measured and the owner says to switch.

## What a runner machine must have

- **bwrap** (bubblewrap). The gate refuses to run without it: the candidate's
  build, the scenarios and the checks all run inside it.
- **The same Rust toolchain as the primary**, installed through rustup under
  the runner user's `$HOME` (the gate's sandbox binds it back read-only).
  The gate's base-cache key includes `rustc -vV`, `cargo -V`, `node -V`,
  `psql -V`, `git --version`, `tmux -V`, `python3 -V`, `jq --version` and the
  test server's version, so a different toolchain does not share the cache:
  it computes a different key. Never a false hit — just no sharing.
- **A private PostgreSQL**: `docker` with the test server's image
  (`postgres:16-alpine`, the default when docker is present), or the host's
  own programs in `SPAWN_GATE_PG_BIN` form (`/usr/lib/postgresql/16/bin`).
  A runner never uses a shared test database and never receives one's URL.
- **git, tar, gzip, sha256sum, jq, nice**, GNU coreutils' **timeout** (8.5 or
  later, for `--kill-after`), `nproc`, and a POSIX shell.
- **Disk**: the gate's floor (`SPAWN_GATE_MIN_FREE_GB`, default 10) in the
  runner root's TMPDIR, plus one scenario's scratch. The private server is
  copied from its template per gate (about 40 MB), and the build is the same
  build a gate does anywhere.
- **CPU**: gates are CPU- and memory-bound and IO-free; a runner with fewer
  cores than the primary simply takes fewer scenarios at once
  (`SPAWN_GATE_JOBS`, half the cores at most 8 by default).
- **The UI's dependencies**, if the gate is to run `check-web.sh`: install
  the controller's `web/` dependencies in a checkout and point setup at it
  (`--web <checkout>/web`). Without it the check reports the gap and
  `check-web.sh` fails on the runner.
- SSH access from the primary (key-based, no password; the dispatcher uses
  `BatchMode=yes`).
- A root **not** under `/tmp`, `/run`, `/var/tmp` or `TMPDIR`: the gate's
  sandbox empties those and would not see the controller's scripts inside
  the job (under `$HOME` it binds them back; elsewhere the sandbox shows the
  host's files). `setup.sh` refuses such a root and `--check` reports it.
  Keep it short, too: a Unix socket path is at most 107 bytes, and the
  gate's own server puts one about 40 bytes below `TMPDIR`; a root whose
  path is too long moves the job's scratch to `/tmp/gate-runner/<job>` (and
  then the disk floor is measured on `/tmp`).

What it must **not** have: no engines, no workspaces, no tmux sessions, no
live database. `setup.sh` wants nothing of the fleet, and the dispatcher
never sends one: the job carries code and commits, never a credential.

## Installing a runner

On the machine, from a login shell whose `PATH` finds rustc/cargo:

```sh
deploy/gate-runner/setup.sh --web ~/spawn-checkout/web
```

It writes `~/.local/share/gate-runner/config` (mode 0600), installs
`~/.local/share/gate-runner/bin/remote`, makes `jobs/`, `cache/` and `work/`,
links `web/node_modules` in, and ends by printing the preflight report
(`remote preflight`), the same one `--check` reads. Options:
`--root`, `--pg-image`, `--pg-bin`, `--jobs`, `--min-free-gb`, `--nice`;
`--force` rewrites a config the owner may have tuned. A plain rerun keeps the
config and reinstalls only the helper.

The helper's sha256 is the protocol version: the dispatcher refuses a runner
whose installed helper differs from its own copy, so **upgrading the runner
means re-running setup.sh** from the new tree.

## Dispatching a gate

```sh
deploy/gate-runner.sh <runner> <source.git> <base> <candidate> <scenarios> <out>
```

`<source.git>` is a bare repository holding both commits (the missions repo);
`<scenarios>` is the controller's scenario directory (its tree's `scripts/`
are shipped with it); `<out>` receives `<out>/<job>/`:

- `verdict.json` — the gate's own one-line verdict, when it wrote one (a
  no-room, an abort or a setup failure leaves none);
- `gate.log` — the whole gate output, stdout and stderr;
- `record.json` — the job, the resolved commits, and every attempt with its
  id, its stages, its gate exit and its result.

The job id is the candidate's first 12 characters and a nonce; every
transport retry is a new attempt (`<job>.a1`, `.a2`, …) with its own runner
directory. Retries are for the transport only. A gate that ran and answered
is the answer: a rejection is never retried. A missing verdict or a verdict
that names other commits is a failure (exit 2), never a pass.

Exit status is the gate's own when the gate answered: 0 with a verdict
(`verdict.json` says `accepted` or `rejected`), 2 abort, 3 setup, 4
harness, 5 no room, 6 host fault or cannot_judge. It is 2 when no whole
result could be brought back (transport after the retries, a stage's
timeout, a missing result), and 3 when the runner is not ready.

Stages and their limits (flags override the environment defaults):
`--preflight-timeout` (300 s), `--ship-timeout` (900 s), `--run-timeout`
(7200 s), `--collect-timeout` (600 s), all under `--total-timeout`
(10800 s). The runner also runs the gate under its own `timeout`, so a
broken SSH connection cannot leave a gate running there. `--retries N`
(default 2) new attempts after a transport failure. `--keep` keeps the
runner's job directory (on failure it is kept anyway, for a look);
`--remote-bin` and `--ssh` name the helper's path and the transport.

## The compatibility check

```sh
deploy/gate-runner.sh --check <runner> <source.git> <base> <scenarios>
```

It reports what the runner has — helper version, git/tar/gzip/sha256sum/
nice/timeout/bwrap, the private server, free disk against the floor, the
UI's lockfile against the controller's, the toolchain — and then asks the
question the base cache depends on: **does the runner compute the same
base-cache key this host does?** It ships the archive, runs
`gate.sh --base-keys` on the runner and the same script here, and compares
the keys scenario by scenario (the mode runs the gate's setup — the exports,
the socket scan and the toolchain probe — and no scenario). Equal keys mean
a base result cached here is a hit there; different keys mean the toolchain,
the server, `LANG` or a bound directory differs, and the two caches do not
share (the difference is printed; `--keep` keeps the runner's log).

On the primary, run it with the environment the integrator gives a gate
(`--env-file ~/.config/spawn/gate-env`, as `deploy/deploy.sh` reads it), not
your shell's: otherwise the comparison is about a gate nobody runs.

Exit 0: compatible. 1: not. 3: the runner could not be asked.

## The protocol (for a reader of `remote.sh`)

`remote version | preflight | accept <job> | run <job> <seconds> |
keys <job> <seconds> | collect <job> | remove <job>`. `accept` takes the
job's tar.gz on stdin and installs it whole or not at all; `collect` writes
a MANIFEST of sha256s and the result files as one tar.gz on stdout; the
dispatcher verifies the manifest before it believes anything. The runner's
root is the directory two levels above the helper; `config` there names the
jobs, the server, the disk floor, the nice level and the PATH the gate runs
with.

## Limits and known costs

- The whole history of both commits is shipped in a bundle (Spawn's is about
  15 MiB packed): the gate exports trees, and a bundle is cheaper than a
  checkout. A LAN hop is not the cost of a gate.
- Two gates on the same runner share its base cache, with the gate's
  per-key lock; two gates on the same key anywhere are the same run twice.
- Everything the gate runs on the runner is the primary's shipped scripts;
  the runner must trust the primary. The primary's verdict file is only
  believed from the runner's `out/verdict`, and the gate's semantics decide
  it, not the helper.
- A transport failure during the run can leave the gate running on the
  runner until the runner's own `timeout` ends it; the retry ships a new
  attempt with its own id and directory, so nothing is overwritten.
- The runner's disk is its own: jobs are removed after a successful
  collection, and a failed attempt's job is kept for a look (remove it by
  hand, or with `remote remove <job>`).
