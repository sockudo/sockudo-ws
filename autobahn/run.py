#!/usr/bin/env python3
"""Run the bundled Autobahn suite against sockudo-ws, preserving its exit status."""

import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parent.parent
REPORTS = ROOT / "autobahn" / "reports"
# Cargo supports a shared target directory; resolve relative paths from the
# autobahn directory, where both Makefile build commands are invoked from.
TARGET = os.environ.get("CARGO_TARGET_DIR")
SERVER_TARGET = Path(TARGET).resolve() if TARGET else ROOT / "target"
RUNNER_TARGET = Path(TARGET).resolve() if TARGET else ROOT / "autobahn-testsuite-rs" / "target"


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def main():
    signal.signal(signal.SIGTERM, interrupted)
    REPORTS.mkdir(parents=True, exist_ok=True)
    with (REPORTS / "server.log").open("w") as log:
        server = subprocess.Popen(
            [str(SERVER_TARGET / "release" / "autobahn-server")],
            cwd=ROOT,
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + 15
            while True:
                if server.poll() is not None:
                    raise RuntimeError("Echo server exited; see autobahn/reports/server.log")
                # Check this process reached its listening state, so an unrelated
                # service already on port 9001 cannot satisfy the readiness check.
                if "Ready for Autobahn test suite" in (REPORTS / "server.log").read_text():
                    with socket.create_connection(("127.0.0.1", 9001), timeout=1):
                        break
                if time.monotonic() >= deadline:
                    raise RuntimeError("Echo server did not become ready within 15 seconds")
                time.sleep(0.1)

            with (REPORTS / "runner.log").open("w") as runner_log:
                with subprocess.Popen(
                    [
                        str(RUNNER_TARGET / "release" / "wstest"),
                        "--mode", "fuzzingclient",
                        "--spec", "fuzzingclient.json",
                        "--concurrency", "8",
                    ],
                    cwd=ROOT / "autobahn",
                    stdout=subprocess.PIPE,
                    stderr=subprocess.STDOUT,
                    text=True,
                ) as runner:
                    try:
                        for line in runner.stdout:
                            print(line, end="", flush=True)
                            runner_log.write(line)
                            runner_log.flush()
                        return runner.wait()
                    finally:
                        if runner.poll() is None:
                            runner.terminate()
                            try:
                                runner.wait(timeout=5)
                            except subprocess.TimeoutExpired:
                                runner.kill()
                                runner.wait()
        finally:
            server.terminate()
            try:
                server.wait(timeout=5)
            except subprocess.TimeoutExpired:
                server.kill()
                server.wait()
            print(f"Autobahn reports and logs: {REPORTS}")


if __name__ == "__main__":
    sys.exit(main())
