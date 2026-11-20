#!/usr/bin/env python3
"""Replay the inbox on a synthetic store under an isolated Xvfb and Weston."""

import argparse
from contextlib import closing, contextmanager
import json
import math
import os
from pathlib import Path
import re
import sqlite3
import subprocess
import sys
import tempfile
import time


@contextmanager
def running(command, **kwargs):
    with subprocess.Popen(command, **kwargs) as child:
        try:
            yield child
        finally:
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    child.kill()


def wait_for(child, description, condition, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline and child.poll() is None:
        result = condition()
        if result:
            return result
        time.sleep(0.03)
    raise RuntimeError(f"timed out waiting for {description} (exit status {child.poll()})")


def copy_fixture(source, path):
    with closing(sqlite3.connect(source.as_uri() + "?mode=ro", uri=True)) as db:
        count, examples = db.execute(
            "SELECT count(*), sum(from_email LIKE '%@bench.test') FROM messages"
        ).fetchone()
        if (count, examples) != (100_000, 100_000):
            raise ValueError("expected the synthetic 100,000-message fell-bench fixture")
        with closing(sqlite3.connect(path)) as copy:
            db.backup(copy)


def read_events(path):
    # The last line may be in progress while the app is still running.
    complete = path.read_text().rpartition("\n")[0]
    return [json.loads(line) for line in complete.splitlines()]


def describe(name, samples):
    if not samples:
        print(f"{name}: no samples")
        return
    samples = sorted(samples)
    p50 = samples[math.ceil(len(samples) * 0.50) - 1]
    p95 = samples[math.ceil(len(samples) * 0.95) - 1]
    print(f"{name}: {len(samples)} keys, p50 {p50:.3f} ms, p95 {p95:.3f} ms, "
          f"max {samples[-1]:.3f} ms")
    return p95


def report(events, expected, factor):
    received = {e["key_id"]: e["action"] for e in events if e["event"] == "key_received"}
    painted = {e["key_id"]: e["latency_ms"] for e in events if e["event"] == "key_painted"}
    presented = {e["key_id"]: e["latency_ms"] for e in events if e["event"] == "key_presented"}
    startup = {e["event"]: e["at_ms"] for e in events if "key_id" not in e}
    for name in (
        "window_created", "first_paint", "first_frame_presented",
        "usable_inbox_paint", "usable_inbox_presented",
    ):
        if name in startup:
            print(f"{name.replace('_', ' ')}: {startup[name]:.3f} ms")
    describe("CPU paint", painted.values())
    frame_p95 = describe("Compositor presentation", presented.values())
    expected_actions = ["Next"] * 200 + ["Open conversation", "Page down", "Back to list",
                                          "Archive", "Undo"]
    if (len(received) != expected or painted.keys() != received.keys()
            or list(received.values())[:len(expected_actions)] != expected_actions):
        raise RuntimeError(f"incomplete replay: expected {expected} inbox keys, "
                           f"received {len(received)}, painted {len(painted)}")
    missing = len(received.keys() - presented.keys())
    if missing:
        print(f"Compositor feedback missing for {missing} keys")
    if factor is None:
        return
    usable = startup.get("usable_inbox_presented")
    if usable is None or missing or frame_p95 is None:
        raise RuntimeError("presentation coverage is incomplete")
    if usable > 300 * factor or frame_p95 > 8 * factor:
        raise RuntimeError(f"UI budget exceeded (startup ≤ {300 * factor:g} ms, "
                           f"presentation p95 ≤ {8 * factor:g} ms)")


def replay(args):
    with tempfile.TemporaryDirectory(prefix="fell-ui-") as directory:
        root = Path(directory)
        env = {k: v for k, v in os.environ.items() if not k.startswith("FELL_")}
        env.pop("WAYLAND_SOCKET", None)
        for name in ["HOME", "XDG_CONFIG_HOME", "XDG_CONFIG_DIRS", "XDG_DATA_HOME",
                     "XDG_CACHE_HOME", "XDG_STATE_HOME", "XDG_RUNTIME_DIR"]:
            path = root / name.lower()
            path.mkdir(mode=0o700)
            env[name] = str(path)
        fixture = root / "store.db"
        copy_fixture(args.fixture, fixture)
        env.update(WAYLAND_DISPLAY="fell-replay", FELL_STORE=str(fixture), FELL_WINDOW="1200x780")
        compositor_log = args.output / "weston.log"
        command = ["weston", "--no-config", "--backend=x11", "--shell=kiosk",
                   f"--renderer={args.renderer}", "--width=1200", "--height=780",
                   "--socket=fell-replay", f"--log={compositor_log}"]
        with running(command, env=env) as compositor, open(args.output / "fell.log", "w") as log:
            def window_id():
                log_text = compositor_log.read_text() if compositor_log.exists() else ""
                match = re.search(r"window id (\d+)\s*$", log_text, re.MULTILINE)
                if match and (Path(env["XDG_RUNTIME_DIR"]) / "fell-replay").is_socket():
                    return match[1]

            app_env = dict(env, XDG_SESSION_TYPE="wayland")
            app_env.pop("DISPLAY")
            window = wait_for(compositor, "Weston window", window_id)
            traces = Path(env["XDG_STATE_HOME"]) / "fell" / "trace"
            with running([args.binary, "--trace"], env=app_env, stdout=log, stderr=log) as app:
                path = wait_for(app, "trace file", lambda: next(traces.glob("*.jsonl"), None))
                try:
                    wait_for(app, "usable inbox CPU paint", lambda: any(
                        e["event"] == "usable_inbox_paint" for e in read_events(path)
                    ))
                    # Keep input out of the startup measurement. CPU-only
                    # diagnostics can still run when presentation is absent.
                    try:
                        wait_for(app, "usable inbox presentation", lambda: any(
                            e["event"] in ("usable_inbox_presented", "presentation_unavailable")
                            for e in read_events(path)
                        ), timeout=4)
                    except RuntimeError:
                        if app.poll() is not None:
                            raise
                    subprocess.run(["xdotool", "windowfocus", window, "mousemove",
                                    "--window", window, "8", "55", "click", "1"],
                                   env=env, check=True, timeout=10)
                    batches = [(45, ["j"] * 200), (300, ["Return", "ctrl+d", "Escape", "e", "u"])]
                    if args.search:
                        batches.append((70, ["slash", *"boile"]))
                    for delay, keys in batches:
                        subprocess.run(["xdotool", "key", "--delay", str(delay), *keys],
                                       env=env, check=True, timeout=30)
                    expected = sum(len(keys) for _, keys in batches)
                    wait_for(app, "final CPU paint", lambda: sum(
                        e["event"] == "key_painted" for e in read_events(path)
                    ) >= expected, timeout=4)
                    try:
                        wait_for(app, "final presentation feedback", lambda: sum(
                            e["event"] == "key_presented" for e in read_events(path)
                        ) >= expected, timeout=2)
                    except RuntimeError:
                        if app.poll() is not None:
                            raise
                    report(read_events(path), expected, args.ci_factor)
                finally:
                    (args.output / "trace.jsonl").write_bytes(path.read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=lambda p: Path(p).resolve(strict=True))
    parser.add_argument("fixture", type=lambda p: Path(p).resolve(strict=True))
    parser.add_argument("output", type=Path, help="new directory for logs and the trace")
    parser.add_argument("--renderer", choices=("pixman", "gl"), default="pixman")
    parser.add_argument("--search", action="store_true", help="replay slash search in builds with it")
    parser.add_argument("--ci-factor", type=float, help="enforce the 300/8 ms budgets times this factor")
    parser.add_argument("--inside-xvfb", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.ci_factor is not None and not (math.isfinite(args.ci_factor) and args.ci_factor > 0):
        parser.error("--ci-factor must be finite and positive")
    if args.inside_xvfb:
        args.output.mkdir(mode=0o700)
        replay(args)
    else:
        return subprocess.run([
            "xvfb-run", "-a", "-s", "-screen 0 1200x780x24 -nolisten tcp",
            sys.executable, __file__, *sys.argv[1:], "--inside-xvfb",
        ]).returncode


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, RuntimeError, sqlite3.Error, subprocess.SubprocessError) as error:
        sys.exit(f"inbox replay: {error}")
