# Memory

Measured with a release build on WSL2 in a 130x30 terminal. The numbers are
`VmRSS` from `/proc/<pid>/status`, after the list finished loading:

| state | RSS |
|---|---|
| idle, Liked Music loaded | ~8 MB |
| playing a normal track | ~11.5 MB |
| playing a 108 h, 6.3 GB mix | ~16.5 MB |
| the same after seeking to the middle | ~16.5 MB |

Peak memory (VmHWM) was at most 1.5 MB above RSS.

The 108 h case is the largest because it keeps the file's fragment index in
RAM (about 0.6 MB, 39k fragments) so seeking stays a single request. Apart
from that index, memory is a roughly 1 MB audio ring, up to six 256 KiB range
chunks (four queued ahead of the decoder and two in flight, about 1.5 MB), the
packet being decoded and up to 16 resolved URLs (about 25 KiB). How the audio
pipeline is structured is in [audio.md](audio.md).

A tail scan borrows a second decoder for a moment at the end of a track: about
0.4 MB of transient RSS, and under 1 KiB of RMS levels. It does not change the
steady-state numbers above.

## The resolver spike

These are ytui's own numbers. Resolving a track also runs `yt-dlp`, which
starts deno to solve YouTube's JavaScript challenge: about 110 MB plus 300 MB
for two or three seconds, once per track. That is the spike a system monitor
shows when a track is resolved, and it belongs to those processes, not to the
player; on Windows the yt-dlp child alone measures about 70 MB of working set
before deno starts, which is not comparable to the `VmRSS` figures above.
Skipping to the track that is already being resolved waits for that resolution
instead of starting a second one.

The [remote resolver](resolver.md) exists for that spike: with `resolver_url`
set, those processes run on the server instead and the player numbers in the
table do not change.

## Virtual size

The virtual size is bigger still (about 350 MB) because PulseAudio maps a few
64 MB shared-memory pools and glibc reserves heap for each thread. Almost none
of that is resident, so RSS is the number to watch.

## Measuring yourself

`python3 tools/ram.py [URL ...]` runs the real TUI in a pty and prints the same
table on your machine, plus the peak of the resolver processes.
