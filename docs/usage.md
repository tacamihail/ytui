# Usage

The six tabs, the keyboard and the mouse. Press `?` in the player to see the
key list at any time.

## Tabs

1. Liked Music (opens here)
2. Playlists
3. Subscriptions
4. History
5. Queue
6. Search

`Enter` on a playlist opens it, `Esc` goes back. History is local
(`~/.local/share/ytui/history.json`); Queue is whatever is playing now.

Liked Music, Playlists and Subscriptions are also cached in
`~/.config/ytui/cache/`, so switching to a tab paints the last list
immediately. A cache older than 15 minutes is refreshed in the background
while the old rows stay on screen; `r` refreshes right away. On a cold start,
`--lazy-playlist` streams the first page as yt-dlp reads it, so rows fill in
progressively instead of all appearing at once.

## Keys

`1`-`6`, `Tab` and `Shift+Tab` switch tabs. `Up`/`Down` or `j`/`k` move the
selection, `PageUp`/`PageDown` jump, `Enter` plays the list from the selected
row, `a` appends to the queue, `/` searches (a query, video URL or playlist
URL) and `r` refreshes.

`space` or `p` pauses, `left`/`right` seek 5 seconds, `,` and `.` seek 30,
`[` `]` change the volume, `m` mutes, `s` shuffles, `R` cycles repeat, `<` `>`
skip tracks, `x` stops, `d` deletes an entry, `c` copies the link, `?` shows
the help (with the version and an update check), `u` installs a newer version
(or checks when there is none), and `q` quits. Inside the help overlay, `s`
scans the local network for a resolver.

## Mouse

Click a tab, click a row to select it, double-click to play, click the
progress bar to seek, and click the buttons in the player. The wheel moves
five rows at a time. The speaker at the end of the player line mutes, and the
slider to its left can be clicked or dragged.

The row that is playing is marked with a green ▶ in every list that contains
it, so it is easy to find your place after browsing elsewhere.

## Media keys

ytui registers with the operating system's media session, so the keyboard's
play/pause, next and previous keys, and headphone buttons, control playback
even while the terminal is in the background:

- **Windows**: System Media Transport Controls (the volume flyout and lock
  screen).
- **Linux**: MPRIS as `org.mpris.MediaPlayer2.ytui`, which GNOME, KDE and
  `playerctl` use for their media widgets and key bindings.
- **macOS**: Now Playing (`MPNowPlayingInfoCenter` and `MPRemoteCommandCenter`),
  which also feeds Control Center and, on Touch Bar Macs, the Control Strip.
  ytui loads the YouTube thumbnail as artwork.
- **WSL**: Windows delivers the keys to Windows programs only, so the WSL
  build starts the native Windows build (`ytui.exe --media-bridge`) and drives
  its media session over a pipe. Put `ytui.exe` on `PATH` (the Windows install
  does this), or point `YTUI_MEDIA_BRIDGE` at it. If WSL interop is disabled,
  ytui falls back to grabbing the XF86 keys from WSLg's X server.

The keys work while the help or search overlay is open, and terminals that
forward media keys as input are handled too. `ytui doctor` prints which
session was selected. Set `media_keys` to `false` in the config to turn the
integration off.

## Platform niceties

- On macOS the current track appears in the system Now Playing panel — see
  [Media keys](#media-keys) for what controls it.
- On Windows the audio follows the system default output device: connecting
  Bluetooth headphones mid-song moves the music to them — see
  [audio.md](audio.md#output-device-windows).

## When a track won't play

When a track can't be resolved because YouTube no longer serves it, ytui says
so in the status line and moves to the next one. It gives up after five dead
tracks in a row, which usually means the problem is cookies or the network
rather than the list; `space` retries.
