#!/usr/bin/env python3
"""Measure ytui's resident memory while idle and while playing.

Starts the real TUI in a pty and reads VmRSS/VmHWM from /proc. Linux only.

    python3 tools/ram.py                          # idle only
    python3 tools/ram.py <youtube-url> [...]      # idle + one row per URL

Each URL is pasted into the search box and starts playing once yt-dlp
resolves it, which takes about 20 seconds.
"""

import fcntl
import os
import pty
import re
import select
import struct
import subprocess
import sys
import termios
import threading
import time

COLS, ROWS = 130, 30
SETTLE = 20.0  # give yt-dlp + the first buffer time to arrive


def spawn():
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))

    def leader():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)

    p = subprocess.Popen(
        [os.path.expanduser("~/.local/bin/ytui")],
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=dict(os.environ, TERM="xterm-256color"),
        close_fds=True,
        preexec_fn=leader,
    )
    os.close(slave)
    return p, master


def drain(master, seconds):
    end = time.time() + seconds
    while time.time() < end:
        r, _, _ = select.select([master], [], [], 0.1)
        if master in r:
            try:
                if not os.read(master, 1 << 20):
                    break
            except OSError:
                break


def rss(pid):
    vals = {}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            m = re.match(r"(VmRSS|VmHWM):\s+(\d+) kB", line)
            if m:
                vals[m.group(1)] = int(m.group(2))
    return vals.get("VmRSS", 0) / 1024, vals.get("VmHWM", 0) / 1024


def children_of(pid):
    """Direct and indirect child pids, found through /proc/<pid>/stat."""
    kids = {}
    for name in os.listdir("/proc"):
        if not name.isdigit():
            continue
        try:
            with open(f"/proc/{name}/stat") as f:
                ppid = int(f.read().rsplit(")", 1)[1].split()[1])
        except (OSError, IndexError, ValueError):
            continue
        kids.setdefault(ppid, []).append(int(name))
    out, todo = [], [pid]
    while todo:
        for c in kids.get(todo.pop(), []):
            out.append(c)
            todo.append(c)
    return out


def tree_rss(pid):
    """RSS of everything below pid: yt-dlp and its deno while resolving."""
    total = 0.0
    for c in children_of(pid):
        try:
            total += rss(c)[0]
        except OSError:
            pass
    return total


def play(master, pid, url):
    """Paste a URL, start it, and return the peak RSS of the resolver."""
    peak = [0.0]
    stop = threading.Event()

    def sample():
        while not stop.is_set():
            peak[0] = max(peak[0], tree_rss(pid))
            time.sleep(0.1)

    th = threading.Thread(target=sample)
    th.start()
    os.write(master, b"/")
    drain(master, 0.3)
    os.write(master, url.encode())
    drain(master, 0.3)
    os.write(master, b"\r")
    drain(master, SETTLE)
    stop.set()
    th.join()
    return peak[0]


def main():
    if not os.path.isdir("/proc"):
        print("this tool needs Linux /proc", file=sys.stderr)
        return 1
    urls = sys.argv[1:]

    p, master = spawn()
    drain(master, 8.0)
    rss_now, peak = rss(p.pid)
    rows = [("idle, Liked Music loaded", rss_now, peak)]
    resolver = 0.0

    for url in urls:
        resolver = max(resolver, play(master, p.pid, url))
        rss_now, peak = rss(p.pid)
        label = url if len(url) <= 36 else url[:33] + "..."
        rows.append((f"playing {label}", rss_now, peak))

    os.write(master, b"q")
    drain(master, 1.0)
    p.terminate()
    try:
        p.wait(timeout=3)
    except subprocess.TimeoutExpired:
        p.kill()

    print(f"{'state':40} {'RSS':>9} {'peak':>9}")
    print("-" * 60)
    for label, rss_now, peak in rows:
        print(f"{label:40} {rss_now:8.1f}M {peak:8.1f}M")
    if urls:
        print(
            f"\nwhile resolving, yt-dlp and deno peaked at {resolver:.0f} MB"
            "\n(separate processes, a few seconds per track)"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
