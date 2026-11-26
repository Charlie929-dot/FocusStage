import json
import os
import selectors
import subprocess
import sys
import time
from pathlib import Path


def verify(exe):
    exe = Path(exe).absolute()
    with subprocess.Popen(
        [exe, "--window-host"], cwd=exe.parent, stdin=subprocess.PIPE, stdout=subprocess.PIPE
    ) as host, selectors.DefaultSelector() as selector:
        try:
            selector.register(host.stdout, selectors.EVENT_READ)
            deadline = time.monotonic() + 10
            output = bytearray()
            while b"\n" not in output and len(output) < 4096:
                if not selector.select(max(0, deadline - time.monotonic())):
                    raise ValueError("timed out waiting for the window host handshake")
                chunk = os.read(host.stdout.fileno(), 4096)
                if not chunk:
                    break
                output.extend(chunk)
            line, newline, _ = output.partition(b"\n")
            if not newline:
                raise ValueError("missing window host handshake")
            ready = json.loads(line)
            if ready["type"] != "ready" or not all(ready[key] > 0 for key in ("scale", "width", "height")):
                raise ValueError("invalid window host handshake")
            if host.poll() is not None:
                raise ValueError("window host exited after the handshake")
        finally:
            host.stdin.close()
            try:
                host.wait(timeout=2)
            except subprocess.TimeoutExpired:
                host.kill()


if __name__ == "__main__":
    try:
        verify(sys.argv[1])
    except (OSError, ValueError, TypeError, KeyError) as error:
        sys.exit(f"FocusStage.exe verification failed: {error}")
