# timing-shim (scripts/timing): the engine, with its output reaching the
# pane through the lagging relay (relay.py). The engine stays the runner's
# direct child: a harness that kills the runner's child kills the engine.
exec > >(exec python3 -I "$D/relay.py" "$D" "${SPAWN_EXECUTION_ID:-$$}") 2>&1
exec "$@"
