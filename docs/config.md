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

Claude Code reads an empty slot as unset, so an empty slot falls back on
Claude Code's built-in model. It never falls back on a value from the host
file. The top-level `model` key is pinned only when there is a main model to
pin. Without `SPAWN_DEFAULT_MODEL`, a top-level `model` in
`~/.claude/settings.json` still picks the default profile's main model. Set
`SPAWN_DEFAULT_MODEL`, or use section 2, to close that gap.

Spawn never sets credentials. `ANTHROPIC_AUTH_TOKEN` still comes from the
engine's Claude Code directory. Without a relay, Spawn sets no model keys.

`spawn doctor --json` lists the pinned slots under
`model_endpoints.pinned_slots`.

## 2. A fleet-owned Claude Code directory (opt-in)

Point engines at a directory the fleet owns, so they never read
`~/.claude/settings.json`, its plugins, or its memory:

```sh
SPAWN_CLAUDE_CONFIG_DIR=/srv/spawn/claude    # environment, absolute path only
```

```toml
# spawn.toml (absolute or ~/ path; resolves to SPAWN_CLAUDE_CONFIG_DIR)
[engines.claude]
config_dir = "~/.claude-spawn"
```

When it is set:

- engines get it as `CLAUDE_CONFIG_DIR`. Under `BwrapView` with
  `SPAWN_CLAUDE_SANDBOX=1`, it is the host directory whose `settings.json`,
  `.credentials.json` and `plugins/` are bind-mounted read-only into each
  worker, and whose `projects/` receives the transcripts;
- its sensitive files are denied to the engine just like `~/.claude`'s;
- transcripts default to `$SPAWN_CLAUDE_CONFIG_DIR/projects`;
- its `settings.json` must hold the gateway token. Nothing else should
  write to it.

A relative path stops `spawn loop`. When it is unset or empty, behaviour is
unchanged: engines use `~/.claude`.

`spawn doctor` shows which directory engines use. It prints the path, never
the contents:

```
claude_config: source=SPAWN_CLAUDE_CONFIG_DIR, path=/srv/spawn/claude, isolated=true, settings_json=present
```

`source` is `SPAWN_CLAUDE_CONFIG_DIR`, `[engines.claude].config_dir`, or
`default` (`~/.claude`). Doctor warns when the path is invalid, and when an
isolated directory has no `settings.json`.
