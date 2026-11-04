# Storage: one place, reachable from anywhere

Status: proposal (2026-11-04). The goal: everything Spawn knows lives in one
store, and any machine with the right credentials can run compute against it
(a Bridge on another host, the dashboard on a laptop, a second integrator).

## Where state lives today

| State | Where | Rebuildable? |
| --- | --- | --- |
| Event log: sessions, commands, observations, missions, tasks, residents, gate verdicts, landings | PostgreSQL `spawn_events` (+ folds), one container on this host, bound to 127.0.0.1 | It *is* the source |
| What an engine did (prompts, tools, turns, errors) | Read by the Bridge from the engine's transcript and pane into the log | Yes: in the log |
| The full transcript text | `~/.claude/projects/...` on the compute host | **No** — lost with the host |
| Reports | Collected into `mission_tasks.report`; a copy kept in the workspace | Yes |
| Code: mission sources | Bare repos (`missions/*.git`) or remote URLs | Remote URLs: yes; local bare repos: **no** |
| Code: agents' work in progress | Workspaces and per-agent branches in host-local mirrors (`ws/.sources/*.git`) | **No** — checkpoint commits exist only on the host |
| Backlog, scenarios, gate script | Files in the controller's checkout, referenced from a mission by absolute path | **No** on another host |
| Logs | journald on the host | No (diagnostic only) |
| Secrets | `spawn.env`, `~/.config/spawn/*`, mode 600 | Deliberately not stored |

So the log is already the single source of truth for *decisions*; what is
not yet central is **code in progress, controller inputs (backlog,
scenarios), and full transcripts**, and the database itself is reachable only
from this machine.

## Proposal

1. **Code goes to a remote.** A mission's source is a URL (it already may
   be). The Bridge pushes every checkpoint commit of an agent branch to that
   remote (`refs/spawn/<agent>`), and records the commit in the log as it does
   now. Any host can resume an agent from the remote branch; the local mirror
   becomes a cache.
2. **Controller inputs are versioned, not paths.** A mission's gate names a
   repository URL + commit (or a path inside the source repo's protected
   area) instead of an absolute directory; the integrator checks out that
   commit before judging. Changing scenarios is then a commit, visible in the
   log, and any integrator host judges the same way.
3. **Transcripts are stored.** The Bridge already reads every line; it also
   stores the raw lines (compressed, per execution) in the database or an
   object store, keyed by execution. Judgements and audits can then replay
   what the agent saw from storage alone.
4. **The database is reachable from anywhere**, with TLS and per-role
   credentials (the restricted logins' passwords are already derived from
   the owner's and no longer fixed). This needs a client TLS stack in
   `spawn-postgres` (it connects with `NoTls` today) unless the network
   itself is private and encrypted.

## The decision that is not technical

Where the one store runs and how it is reached:

| Option | What it takes | Trade-off |
| --- | --- | --- |
| Private network (Tailscale/WireGuard) to this Postgres | Install the network on each host; bind Postgres to the private address | No code change for TLS; this host stays the single point of failure |
| Managed PostgreSQL (Neon, Supabase, RDS, ...) | TLS client in `spawn-postgres`; move the data once | Off-host durability and backups; a bill and a provider |
| Cloudflare Tunnel + Access in front of this Postgres | `cloudflared` on each client host | Reachable from anywhere without opening a port; this host stays the store |
| Expose this Postgres to the internet | TLS, firewall, strong passwords | Simplest; largest attack surface |

Object storage for transcripts and git bundles (if wanted) pairs with any of
them (R2, S3, or a `bytea` table to start).
