# ytui

A music-only YouTube player that lives entirely in your terminal.

No browser, no Electron, no mpv, no video window, no background daemon. One
small Rust binary draws the UI with `ratatui`, streams AAC straight from
`googlevideo.com`, and decodes it with `symphonia` into `cpal`. `yt-dlp` is
only spawned to list results and resolve a stream URL — it exits immediately
afterwards.

```
┌─ 1 Liked Music·412 ─ 2 Playlists ─ 3 Subscriptions ─ 4 History ─ 5 Queue·2 ─ 6 Search·30 ── ◉ you ┐
├─ player ─────────────────────────────────────────────────────────────────────────────────────────┤
│ ⏮  ⏸  ⏭  Bullet For My Valentine — Tears Don't Fall                                     ⤨ 100%  │
│ ━━━━━━━━━━━━━━━●────────────────────────────────────────────────────── 1:23 / 6:45  Bullet…     │
├─ Liked Music ────────────────────────────────────────────────────────────────────────────────────┤
│ ▶ Song A                                                              Artist · 3:45              │
│   Song B                                                              Artist · 4:01              │
└──────────────────────────────────────────────────────────────────────────────────────────────────┘
 1-6 tabs · / search · enter play list · a queue · space pause · ←→/h l seek · , . ±30s · [ ] vol …
```

## RAM

| what | resident |
|---|---|
| ytui (UI + decoder + audio ring) | **~15–25 MB** |
| yt-dlp (only while listing / resolving) | transient, exits |
| mpv / browser / daemon | none |

The player never downloads a whole track. It keeps **one HTTP Range chunk**
(256 KiB by default) plus a **2 s audio ring** (~1 MiB), so a 3-hour mix costs
the same RAM as a 3-minute song.

## Music only

There is no video path at all. yt-dlp returns the format list and ytui picks
the best **AAC/MP4** stream (`itag` 141 → 140 → 139), probing each URL with a
1 KiB Range request because YouTube randomly returns `403` for the 128 kbps
variant. Opus is skipped with a visible message — `symphonia` has no Opus
decoder, and pulling in a second codec would cost more RAM than it is worth.

## Install

Requirements: a Rust toolchain, `yt-dlp`, **deno** (yt-dlp uses it to solve
YouTube's JS challenges) and ALSA development headers to build
(`libasound2-dev` on Debian/Ubuntu, `alsa-lib` on Arch). At runtime it needs
`libasound2`; on WSL it also needs `libasound2-plugins` (the ALSA→PulseAudio
bridge).

```bash
git clone git@github.com:<you>/ytui.git && cd ytui
./install.sh
```

`install.sh` detects your package manager, installs a current `yt-dlp` and
`deno` into `~/.local/bin` / `~/.deno/bin`, installs the ALSA headers (and
`libasound2` / `libasound2-plugins` when they are missing), builds with
`cargo build --release`, installs `~/.local/bin/ytui` (a small launcher over
`ytui.bin`) and seeds `~/.config/ytui/config.json`.

Manual build:

```bash
cargo build --release
install -Dm755 target/release/ytui ~/.local/bin/ytui
```

### WSL

WSL2 has no sound card, only a PulseAudio socket (`$PULSE_SERVER`). `install.sh`
installs the ALSA→PulseAudio plugin automatically on Debian/Ubuntu; if you set
things up by hand, run:

```bash
sudo apt-get install -y libasound2-plugins
```

Then run `ytui doctor`; it should print `✔ audio output`.

If you cannot install the package, extract it locally and the launcher finds it
on its own:

```bash
cd /tmp && apt-get download libasound2-plugins
dpkg -x libasound2-plugins_*.deb ~/.local/alsa
```

The `ytui` launcher exports `ALSA_PLUGIN_DIR` when `~/.local/alsa/...` exists,
so audio also works when started as `wsl ... ytui` or from a desktop entry.

## Connect your YouTube account

Everything account-related (Liked Music, your playlists, subscriptions) needs
cookies from a browser that is signed in to YouTube. No Google Cloud project,
no OAuth client, no API key.

```bash
ytui auth /path/to/cookies.txt     # import a Netscape cookies.txt export
ytui auth --browser firefox        # or read cookies from a local browser
ytui auth --status                 # show what is connected
ytui logout                        # forget cookies and account
```

How to get `cookies.txt`:

1. install the **Get cookies.txt LOCALLY** extension (Chrome/Firefox);
2. open <https://music.youtube.com> while signed in;
3. export the `youtube.com` cookies to `cookies.txt`;
4. `ytui auth ~/cookies.txt` (from WSL: `ytui auth /mnt/c/Users/<you>/cookies.txt`).

`ytui auth` verifies the export by reading your **Liked Music** playlist
(`music.youtube.com/playlist?list=LM`, which is private and therefore owned by
you), stores the channel name/id in `~/.config/ytui/account.json`, and copies
the cookies to `~/.config/ytui/cookies.txt` with mode `0600`.

Without cookies the player still works: search and playback are public.

> Subscriptions are fetched by ytui itself (there is no yt-dlp extractor for the
> feed), so that tab needs the **file** import, not `--browser`.

## Tabs

| # | tab | source |
|---|---|---|
| 1 | Liked Music | `music.youtube.com/playlist?list=LM` (private) — the default tab |
| 2 | Playlists | your channel's `/playlists` tab; `Enter` drills into one, `Esc` goes back |
| 3 | Subscriptions | `youtube.com/feed/subscriptions` (private, parsed from the page) |
| 4 | History | local `~/.local/share/ytui/history.json` |
| 5 | Queue | live playback queue |
| 6 | Search | `ytsearchN:<query>` |

## Keys

### Navigation
| key | action |
|---|---|
| `1`–`6`, `Tab` / `Shift+Tab` | switch tabs |
| `↑↓` / `j k` / wheel | move selection |
| `PageUp` `PageDown` `g` `G` | jump |
| click a tab | switch to that tab |
| click / double-click a row | select / play |
| `/` | search box — accepts a query, a video URL or a playlist URL |
| `Enter` | play the whole list from the selected row |
| `a` | append selection to the queue |
| `Esc` | leave a playlist drill-down |
| `r` | refresh the current list |
| `?` | help overlay |
| `q` / `Ctrl+C` | quit |

### Playback
| key | action |
|---|---|
| `space` / `p` | pause / resume |
| `←` `→` / `h` `l` | seek ∓/±5 s |
| `,` `.` | seek ∓/±30 s |
| click progress bar | seek there |
| click ⏮ / ⏸ ⏵ / ⏭ | previous · pause / resume · next |
| `[` `]` | volume ∓/±5 % |
| `m` | mute / unmute |
| `s` | shuffle |
| `R` | repeat off → all → one |
| `<` `>` | previous / next track |
| `x` | stop |
| `d` | remove from history / queue |
| `c` | copy the YouTube link |

## Config `~/.config/ytui/config.json`

| field | default | meaning |
|---|---|---|
| `cookies_file` | `""` | path to cookies.txt (`ytui auth` sets this) |
| `cookies_browser` | `""` | read cookies from a local browser instead |
| `results_count` | `30` | search results |
| `fetch_limit` | `2000` | max entries per playlist / feed |
| `history_limit` | `300` | history entries kept |
| `seek_step` | `5.0` | `←` `→` seek seconds |
| `volume_step` | `5` | `[` `]` step |
| `volume` | `35` | starting volume, percent (playback starts quiet) |
| `buffer_ms` | `2000` | audio ring buffer; lower = less RAM, more underruns |
| `cache_kib` | `256` | HTTP Range chunk held in memory |

## Files

- `~/.config/ytui/config.json` — config
- `~/.config/ytui/cookies.txt` — imported cookies (`0600`)
- `~/.config/ytui/account.json` — channel name + id from `ytui auth`
- `~/.local/share/ytui/history.json` — history

## Troubleshooting

- **`ytui doctor` says no audio device** - on WSL install
  `libasound2-plugins` (or extract it under `~/.local/alsa`; the launcher picks
  that up); on a desktop make sure PipeWire/PulseAudio is running. Start ytui
  through `~/.local/bin/ytui`, not `ytui.bin`, so the plugin path is set.
- **`no AAC stream for this track (opus only)`** — that upload has no M4A
  audio. It is skipped by design; `symphonia` has no Opus decoder.
- **`private — sign-in required`** — re-export cookies and run
  `ytui auth <file>`; cookies expire after a few weeks.
- **Search returns nothing / 403** — update `yt-dlp`; YouTube changes
  frequently.
- **`Unable to extract uploader id`** — `yt-dlp` cannot find `deno`. Run
  `install.sh` or install Deno and make sure `~/.deno/bin` is on `PATH`;
  `ytui doctor` checks both.
- **Subscriptions empty** — the feed is HTML-parsed, so it needs fresh cookies
  and a `youtube.com` (not just `music.youtube.com`) sign-in.
- **Mouse clicks do nothing** — ytui enables mouse reporting itself, but the
  terminal has to forward the events. Windows Terminal and most Linux terminals
  do; inside **tmux** add `set -g mouse on` and restart ytui. Clicking a row
  selects it; double-click (or `Enter`) plays.
- **A track sounds muffled** — it only had 48 kbps HE-AAC (`itag 139`).
  `symphonia` decodes its AAC core but not the SBR high band, so it plays at the
  right speed with duller highs. ytui prefers AAC-LC 256/128 kbps when YouTube
  offers it.
- **A multi-hour mix hangs on "loading"** — YouTube serves those as a
  fragmented MP4 (100+ hours, several GB). ytui detects that layout from the
  first chunk, opens the stream at the first fragment and seeks through the
  file's `sidx` index, so jumping to any point is one request instead of
  walking every fragment. Seeks land on a fragment boundary (~10 s).

## Uninstall

```bash
rm ~/.local/bin/ytui
rm -rf ~/.config/ytui ~/.local/share/ytui
```
