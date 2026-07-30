#!/usr/bin/env python3
"""Drive the agentsync TUI in a real pty and dump the raw escape stream.

Usage: capture.py ROWS COLS KEYS CWD OUT.raw
Env:   CHILD_HOME, CHILD_PATH, CHILD_ARGS (space separated), HOLD (extra seconds)
"""
import fcntl
import os
import select
import struct
import sys
import termios
import time
import pty

ROWS, COLS = int(sys.argv[1]), int(sys.argv[2])
KEYS = sys.argv[3]
CWD = sys.argv[4]
OUT = sys.argv[5]
# Resolved from the environment because HOME is overridden for the demo world,
# which would make a "~" path expand into the fixture instead.
BIN = os.environ["AS_BIN"]
ARGS = os.environ.get("CHILD_ARGS", "").split() if os.environ.get("CHILD_ARGS") else []
HOLD = float(os.environ.get("HOLD", "0.4"))

pid, fd = pty.fork()
if pid == 0:
    os.chdir(CWD)
    os.environ["TERM"] = "xterm-256color"
    os.environ["COLORTERM"] = "truecolor"
    if os.environ.get("CHILD_HOME"):
        os.environ["AGENTSYNC_HOME"] = os.environ["CHILD_HOME"]
    if os.environ.get("CHILD_PATH"):
        os.environ["PATH"] = os.environ["CHILD_PATH"]
    os.execv(BIN, [BIN] + ARGS)

fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

out = bytearray()


def pump(seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([fd], [], [], 0.05)
        if r:
            try:
                out.extend(os.read(fd, 65536))
            except OSError:
                return


pump(1.4)
for key in KEYS:
    os.write(fd, key.encode())
    pump(0.4)
pump(HOLD)

with open(OUT, "wb") as fh:
    fh.write(out)
print(f"{OUT}: {len(out)} bytes", file=sys.stderr)
