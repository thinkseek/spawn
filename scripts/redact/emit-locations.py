#!/usr/bin/env python3
"""Emit credential byte-span locations (objective 175) via sibling secret-values.py.

Invoked by `CollectorMask` as `python3 -I -B <tool>`. Prints JSON metadata
(`sources`, `entries`, `missing`, `unreadable`, `accepted`, `accepted_paths`,
`watch`, `probe`) to stdout and never prints raw secret values.
"""
import importlib.util
import json
import os
import sys


def _load_collector():
    here = os.path.dirname(os.path.abspath(__file__))
    collector_path = os.path.join(here, "secret-values.py")
    spec = importlib.util.spec_from_file_location("secret_values", collector_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"cannot load collector module at {collector_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main(argv):
    home = argv[0] if len(argv) >= 1 else os.path.expanduser("~")
    root = argv[1] if len(argv) >= 2 else "/"
    sv = _load_collector()
    result = sv.locate(home, root)
    print(json.dumps(result))
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except Exception as exc:  # pragma: no cover - structured stderr on failure
        sys.stderr.write(f"emit-locations: {exc}\n")
        raise SystemExit(1)
