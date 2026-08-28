#!/usr/bin/env python3
"""Invoke `hands call` with a JSON payload loaded from a file.

Using subprocess argv avoids nested PowerShell/CMD quote handling that can
corrupt inline JSON on Windows.
"""

from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("tool", help="Hands tool name")
    parser.add_argument("json_file", type=Path, help="Path to the JSON payload")
    parser.add_argument("--hands", default="hands", help="Hands executable/path")
    args = parser.parse_args()

    try:
        raw = args.json_file.read_text(encoding="utf-8-sig")
    except OSError as e:
        parser.error(f"cannot read JSON file '{args.json_file}': {e}")

    try:
        json.loads(raw)
    except json.JSONDecodeError as e:
        parser.error(f"invalid JSON in '{args.json_file}': {e}")

    try:
        return subprocess.run(
            [args.hands, "call", args.tool, f"@{args.json_file}"],
            check=False,
        ).returncode
    except OSError as e:
        parser.error(f"failed to execute '{args.hands}': {e}")

if __name__ == "__main__":
    raise SystemExit(main())
