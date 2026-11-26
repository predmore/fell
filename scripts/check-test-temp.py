#!/usr/bin/env python3
"""Run the fell-core library tests and check their private TMPDIR for leaks."""

import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    repo = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="fell-test-") as temporary:
        root = Path(temporary)
        print(f"fell-* entries before: {sum(1 for _ in root.glob('fell-*'))}", flush=True)
        result = subprocess.run(
            [
                "env",
                "-u", "WAYLAND_DISPLAY",
                "-u", "DISPLAY",
                "-u", "WAYLAND_SOCKET",
                "-u", "SWAYSOCK",
                "cargo", "test", "-p", "fell-core", "--lib",
            ],
            cwd=repo,
            env={**os.environ, "TMPDIR": str(root)},
            check=False,
        )
        entries = sorted(root.iterdir())
        print(f"fell-* entries after: {sum(p.name.startswith('fell-') for p in entries)}")
        if entries:
            print("Test run left temporary entries:", file=sys.stderr)
            for entry in entries:
                print(f"  {entry.name}", file=sys.stderr)
        return result.returncode or int(bool(entries))


if __name__ == "__main__":
    sys.exit(main())
