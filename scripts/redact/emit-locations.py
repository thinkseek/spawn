#!/usr/bin/env python3
"""Emit credential value source stamps and byte-span locations as JSON (issue #102).

Wraps sibling secret-values.py's locate(home, root) entrypoint for spawn-host's
CollectorMask (`archive_mask.rs`). Prints only file metadata, watch/probe paths,
line numbers/JSON pointers, byte spans, transforms, and printable credential
names — NEVER credential values.
"""

import argparse
import importlib.util
import json
import os
import sys


def load_collector(collector_path):
    """Load secret-values.py via importlib because its filename uses a hyphen."""
    spec = importlib.util.spec_from_file_location("secret_values", collector_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load collector module: {collector_path}")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def main(argv=None):
    parser = argparse.ArgumentParser(description="Emit redacted credential locations JSON.")
    parser.add_argument(
        "--home",
        default=os.path.expanduser("~"),
        help="Account home directory (defaults to current user's ~).",
    )
    parser.add_argument(
        "--root",
        default="/",
        help="Filesystem root for absolute credential paths (defaults to /).",
    )
    parser.add_argument(
        "--collector",
        default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "secret-values.py"),
        help="Path to secret-values.py module.",
    )
    args = parser.parse_args(argv)
    if not os.path.isfile(args.collector):
        print(f"emit-locations.py: collector not found at {args.collector}", file=sys.stderr)
        return 2
    try:
        collector = load_collector(args.collector)
        payload = collector.locate(args.home, root=args.root)
        print(json.dumps(payload))
    except Exception as exc:
        print(f"emit-locations.py: failed to locate credential places ({type(exc).__name__})", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
