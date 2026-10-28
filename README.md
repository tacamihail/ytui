# ytui

A terminal player for YouTube Music. Audio only: no browser and no video
window.

ytui asks yt-dlp for the stream URL, then pulls the AAC file over HTTP Range,
decodes it with `symphonia` and writes to ALSA through `cpal`. yt-dlp exits
after resolving, so nothing else is running while you listen.

```
┌─ 1 Liked Music·412 ─ 2 Playlists ─ 3 Subscriptions ─ 4 History ─ 5 Queue·2 ─ 6 Search·30 ─ ◉ you ┐
├─ player ─────────────────────────────────────────────────────────────────────────────────────────┤
│ ⏮  ⏸  ⏭  Bullet For My Valentine - Tears Don't Fall                         ⤨ 🔊 ━━━━━━━━━● 100% │
│ ━━━━━━━━━━━━━━━●────────────────────────────────────────────────────── 1:23 / 6:45  Bullet…      │
├─ Liked Music ────────────────────────────────────────────────────────────────────────────────────┤
│ ▶ Song A                                                              Artist · 3:45              │
│   Song B                                                              Artist · 4:01              │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
 1-6 tabs · / search · enter play list · a queue · space pause · ←→/h l seek · , . 30s · [ ] vol …
```

## Audio

ytui only downloads an audio stream, so the video resolution on YouTube does
not matter. It asks for AAC (`itag` 141, then 140, then 139) and probes each
URL with a 1 KiB request, because YouTube sometimes returns 403 for one of
them.

The output stream is opened at the track's own sample rate. YouTube AAC is
44.1 kHz, so on a 44.1 kHz device nothing is resampled. If the device refuses
that rate, ytui falls back to 48 kHz and resamples.

Working out a stream URL takes `yt-dlp` about four seconds, which used to be
dead air between songs. The next track is resolved while the current one plays
and only its URL is kept (a few hundred bytes, no audio), so the handover
costs about 0.2 s.

Opus (`itag` 251) would be a bit better than 128 kbps AAC, but `symphonia` has
no Opus decoder, so those uploads are skipped with a message.

## Install

You need Rust, `yt-dlp`, `deno` (yt-dlp uses it for YouTube's JS challenges)
and the ALSA headers: `libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch.

```bash
git clone https://github.com/tacamihail/ytui.git
cd ytui
./install.sh
```

The script puts a recent `yt-dlp` and `deno` in `~/.local/bin` and `~/.deno/bin`
if they're missing, builds the release binary and installs `~/.local/bin/ytui`.
That last file is a small launcher around `ytui.bin`; on WSL it points
`ALSA_PLUGIN_DIR` at the PulseAudio plugin when that lives in `~/.local/alsa`.

To build it yourself:

```bash
cargo build --release
install -Dm755 target/release/ytui ~/.local/bin/ytui
```

### WSL

WSL has no sound card, only a PulseAudio socket. Install the bridge:

```bash
sudo apt-get install -y libasound2-plugins
```

If you can't use sudo, extract it into your home directory and the launcher
will find it:

```bash
cd /tmp
apt-get download libasound2-plugins
dpkg -x libasound2-plugins_*.deb ~/.local/alsa
```

`ytui doctor` should then print an `audio output` line instead of an error.

## Account

Liked Music, playlists and subscriptions need cookies from a browser that is
signed in to YouTube. There's no OAuth app and no API key.

```bash
ytui auth ~/cookies.txt      # import a Netscape cookies.txt
ytui auth --browser firefox  # or read them from a local browser
ytui auth --status           # what is connected right now
ytui logout                  # forget the cookies and the account
```

To export the file, install "Get cookies.txt LOCALLY" in Chrome or Firefox,
open <https://music.youtube.com> while signed in and export the `youtube.com`
cookies. `ytui auth` checks them against your Liked Music playlist, saves the
channel in `~/.config/ytui/account.json` and copies the cookies to
`~/.config/ytui/cookies.txt` with mode `600`.

Search and playback work without an account. Subscriptions don't: ytui parses
that page itself, so it needs the cookie file, not `--browser`.

## Tabs

1. Liked Music (opens here)
2. Playlists
3. Subscriptions
4. History
5. Queue
6. Search

`Enter` on a playlist opens it, `Esc` goes back. History is local
(`~/.local/share/ytui/history.json`); Queue is whatever is playing now.

## Keys

`1`-`6`, `Tab` and `Shift+Tab` switch tabs. `Up`/`Down` or `j`/`k` move the
selection, `PageUp`/`PageDown` jump, `Enter` plays the list from the selected
row, `a` appends to the queue, `/` searches (a query, video URL or playlist
URL) and `r` refreshes.

`space` or `p` pauses, `left`/`right` seek 5 seconds, `,` and `.` seek 30,
`[` `]` change the volume, `m` mutes, `s` shuffles, `R` cycles repeat, `<` `>`
skip tracks, `x` stops, `d` deletes an entry, `c` copies the link, `?` shows
the help and `q` quits.

The mouse works as well: click a tab, click a row to select it, double-click
to play, click the progress bar to seek, and click the buttons in the player.
The speaker icon mutes, and the slider next to it can be clicked or dragged.

The row that is playing is marked with a green ▶ in every list that contains
it, so it is easy to find your place after browsing elsewhere.

When a track can't be resolved because YouTube no longer serves it, ytui says
so in the status line and moves to the next one. It gives up after five dead
tracks in a row, which usually means the problem is cookies or the network
rather than the list; `space` retries.

## Config

`~/.config/ytui/config.json` is created by `install.sh`:

| field | default | meaning |
|---|---|---|
| `cookies_file` | `""` | cookie file path, set by `ytui auth` |
| `cookies_browser` | `""` | read cookies from a browser instead |
| `results_count` | `30` | search results |
| `fetch_limit` | `2000` | max entries per playlist or feed |
| `history_limit` | `300` | history entries kept |
| `seek_step` | `5.0` | seconds for `left`/`right` |
| `volume_step` | `5` | percent per `[` `]` |
| `volume` | `35` | starting volume |
| `buffer_ms` | `2000` | audio ring buffer |
| `cache_kib` | `256` | HTTP Range chunk size |

## Files

- `~/.config/ytui/config.json` - config
- `~/.config/ytui/cookies.txt` - imported cookies, mode `600`
- `~/.config/ytui/account.json` - channel name and id
- `~/.local/share/ytui/history.json` - play history

## Memory

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
RAM (about 0.6 MB, 39k fragments) so seeking stays a single request. Apart from
that index, memory is a roughly 1 MB audio ring, one 256 KiB range chunk, the
packet being decoded and the next track's resolved URL, so normal tracks stay
around 12 MB.

These are ytui's own numbers. Resolving a track also runs `yt-dlp`, which
starts deno to solve YouTube's JavaScript challenge: about 110 MB plus 300 MB
for two or three seconds, once per track. That is the spike a system monitor
shows when a track is resolved, and it belongs to those processes, not to the
player. Skipping to the track that is already being resolved waits for that
resolution instead of starting a second one.

The virtual size is bigger still (about 350 MB) because PulseAudio maps a few
64 MB shared-memory pools and glibc reserves heap for each thread. Almost none
of that is resident, so RSS is the number to watch.

`python3 tools/ram.py [URL ...]` runs the real TUI in a pty and prints the same
table on your machine, plus the peak of the resolver processes.

## Problems

No sound on WSL: `libasound2-plugins` is missing. See the WSL section above.
On a desktop, check that PipeWire or PulseAudio is running.

"no AAC stream for this track (opus only)": the upload has no M4A audio and
`symphonia` can't decode Opus.

"unavailable on youtube": the upload was removed, is blocked in your country,
or is otherwise not served to this account. Nothing in ytui can fix it; the
track stays in the list until you unlike it on YouTube.

"private: sign-in required": the cookies expired. Export them again.

Search returns nothing or a 403: `yt-dlp` is too old.

"Unable to extract uploader id": `yt-dlp` can't find `deno`. Make sure
`~/.deno/bin` is on `PATH`.

Mouse does nothing: the terminal has to forward mouse events. Windows Terminal
and most Linux terminals do; in tmux add `set -g mouse on` and restart ytui.

Tracks that sound muffled only had 48 kbps HE-AAC (`itag 139`). Symphonia
decodes the AAC core but not the SBR band, so the highs are missing. ytui
prefers AAC-LC when YouTube offers it.

## Uninstall

```bash
rm ~/.local/bin/ytui ~/.local/bin/ytui.bin
rm -rf ~/.config/ytui ~/.local/share/ytui
```
