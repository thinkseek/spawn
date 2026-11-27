# Engine configuration: keeping engines off `~/.claude/settings.json`

Claude Code merges the inline `--settings` Spawn passes each engine over the
user's own settings files. Any key Spawn leaves out comes from
`~/.claude/settings.json`, and that file can change while the fleet runs
(issue #79: a rewrite to a raw provider model and an expired token stopped
every worker). Spawn has two defences against this.

## 1. Every model slot is pinned when a relay is configured

With `SPAWN_MODEL_RELAY` set, the inline `env` of every engine names
`ANTHROPIC_BASE_URL` and all six model slots, whatever the profile:

| Slot | Cheap profile | Default profile |
| --- | --- | --- |
| `ANTHROPIC_MODEL` | `SPAWN_CHEAP_MODEL` | `SPAWN_DEFAULT_MODEL`, else empty |
| `ANTHROPIC_DEFAULT_OPUS_MODEL` | `SPAWN_CHEAP_MODEL` | `SPAWN_OPUS_MODEL`, else `SPAWN_DEFAULT_MODEL`, else empty |
| `ANTHROPIC_DEFAULT_SONNET_MODEL` | `SPAWN_CHEAP_MODEL` | `SPAWN_SONNET_MODEL`, else `SPAWN_DEFAULT_MODEL`, else empty |
| `CLAUDE_CODE_SUBAGENT_MODEL` | `SPAWN_CHEAP_MODEL` | `SPAWN_SUBAGENT_MODEL`, else `SPAWN_DEFAULT_MODEL`, else empty |
| `ANTHROPIC_DEFAULT_HAIKU_MODEL` | `SPAWN_SMALL_MODEL` | `SPAWN_SMALL_MODEL` |
| `ANTHROPIC_SMALL_FAST_MODEL` | `SPAWN_SMALL_MODEL` | `SPAWN_SMALL_MODEL` |

Claude Code reads an empty slot (and empty top-level `"model"`) as unset, so
default slots fall back on Claude Code's built-in model. Neither `env` slots
nor top-level `"model"` ever fall back on a value from host
`~/.claude/settings.json`.

Spawn never sets credentials. `ANTHROPIC_AUTH_TOKEN` still comes from the
engine's Claude Code directory. Without a relay, Spawn sets no model keys.

`spawn doctor --json` lists the pinned slots under
`model_endpoints.pinned_slots`.

## 2. Default-isolated Claude Code directory (with `inherit` opt-out)

By default, Spawn isolates engines away from host `~/.claude` into a
fleet-owned directory outside workspaces so host `settings.json` edits,
plugins, or memory cannot drift the fleet:

1. `SPAWN_CLAUDE_CONFIG_DIR=/abs/path` (or `[engines.claude].config_dir` in
   `spawn.toml`) uses that explicit directory.
2. When `SPAWN_CLAUDE_CONFIG_DIR` is unset or empty (and ambient
   `CLAUDE_CONFIG_DIR` is unset or points at `~/.claude`), Spawn resolves a
   deterministic managed directory outside `workspaces/`:
   - `$SPAWN_CLAUDE_DATA_DIR/config` when `SPAWN_CLAUDE_DATA_DIR` is set;
   - else `$XDG_STATE_HOME/spawn/claude-config`;
   - else `$HOME/.local/state/spawn/claude-config`.
3. `SPAWN_CLAUDE_CONFIG_DIR=inherit` (or `host`, case-insensitive) explicitly
   opts out and runs engines on host `~/.claude` (`doctor` emits a drift
   warning).

```sh
SPAWN_CLAUDE_CONFIG_DIR=/srv/spawn/claude    # explicit directory (absolute path only)
SPAWN_CLAUDE_CONFIG_DIR=inherit              # opt out to host ~/.claude
```

```toml
# spawn.toml (absolute or ~/ path; resolves to SPAWN_CLAUDE_CONFIG_DIR)
[engines.claude]
config_dir = "~/.claude-spawn"
```

When the default managed directory is active, `spawn loop` startup creates it
with mode `0700` and performs a one-time, noclobber, sanitized bootstrap when
`<managed>/settings.json` is absent and `~/.claude/settings.json` exists:

- copies only authentication keys (`apiKeyHelper`, `env.ANTHROPIC_AUTH_TOKEN`,
  `env.ANTHROPIC_API_KEY`, plus `env.ANTHROPIC_BASE_URL` only when
  `SPAWN_MODEL_RELAY` is unset), never copying model slots or plugins;
- writes `settings.json` and noclobber-copies `.credentials.json` atomically
  with mode `0600`, never logging file contents.

For the resolved isolated directory:

- engines get it as `CLAUDE_CONFIG_DIR`. Under `BwrapView` with
  `SPAWN_CLAUDE_SANDBOX=1`, it is the host directory whose `settings.json`,
  `.credentials.json`, and `plugins/` are bind-mounted read-only into each
  worker, and whose `projects/` receives transcripts;
- non-Claude sandboxed engines (e.g. `agy`) also have
  `~/.local/state/spawn/claude-config` and `$XDG_STATE_HOME/spawn/claude-config`
  masked alongside `~/.claude`;
- its sensitive files are denied to Claude Code via `--settings` deny rules;
- transcripts default to `<resolved-dir>/projects`.

A relative path (other than `inherit` / `host`) stops `spawn loop`.

`spawn doctor` shows which directory engines use (reading metadata only, never
file contents):

```
claude_config: source=default-isolated, path=/home/owner/.local/state/spawn/claude-config, isolated=true, settings_json=present
```

`source` is `SPAWN_CLAUDE_CONFIG_DIR`, `[engines.claude].config_dir`,
`default-isolated`, or `inherit`. Doctor warns when `inherit` is active, when a
path is invalid, and when an isolated directory has no `settings.json`.

Note on sandbox `$HOME` isolation (`BwrapView`): `ViewConfig::from_env`
defaults to `Blacklist` (masking known credential and sibling-engine paths,
including managed `claude-config`), because `Whitelist` rejects workspaces inside
`$HOME` unless `SPAWN_SANDBOX_CANONICAL_WORKSPACE` is configured (as generated
`spawn.toml` profiles do). Set `SPAWN_SANDBOX_HOME_ISOLATION=whitelist` with a
canonical workspace mount for strict allowlist isolation.
