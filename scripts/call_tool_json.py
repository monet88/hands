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

    payload = json.loads(args.json_file.read_text(encoding="utf-8-sig"))
    encoded = json.dumps(payload, ensure_ascii=False, separators=(",", ":"))
    return subprocess.run([args.hands, "call", args.tool, encoded], check=False).returncode


if __name__ == "__main__":
    raise SystemExit(main())
