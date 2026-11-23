# Troubleshooting

The full problem list. If something looks off, `ytui doctor` is the first
stop; for setup see the [README](../README.md).

## No sound on WSL

WSL has no sound card, only a PulseAudio socket; you need the ALSA-PulseAudio
bridge:

```bash
sudo apt-get install -y libasound2-plugins
```

Rerun `./install.sh` if you like; no `~/.asoundrc` is needed. If `ytui doctor`
says it cannot reach PulseAudio, check WSLg (`wsl --update` on Windows) and
that `PULSE_SERVER` points at `/mnt/wslg/PulseServer`. On a desktop, check that
PipeWire or PulseAudio is running.

## "no AAC stream for this track (opus only)"

The upload has no M4A audio and `symphonia` can't decode Opus — see
[audio.md](audio.md).

## "unavailable on youtube"

The upload was removed, is blocked in your country, or is otherwise not served
to this account. Nothing in ytui can fix it; the track stays in the list until
you unlike it on YouTube.

## "private: sign-in required"

The session has no account cookies. Export them again and import them on this
machine only.

## "playlist not found" on Liked Music or Watch Later

The export is signed out, revoked or incomplete. Netscape exports need the
HttpOnly cookies too ("Get cookies.txt LOCALLY" includes them), so export while
<https://music.youtube.com> is open in a signed-in browser and run
`ytui auth <file>` again. If the export was fresh, check for a second copy:
another ytui (WSL counts), a browser tab still using that session, or a
resolver started with `--cookies`. `ytui doctor` and `ytui auth --status`
check the cookies against YouTube and now say "signed out" instead of
"expired" when YouTube cleared the first-party cookies. The export procedure
is in [account.md](account.md).

## Search returns nothing or a 403

`yt-dlp` is too old.

## "Unable to extract uploader id"

`yt-dlp` can't find `deno`. Make sure `~/.deno/bin` (Windows:
`%USERPROFILE%\.deno\bin`) is on `PATH`.

## "resolver unavailable (...), using local yt-dlp"

The configured resolver did not answer and the client fell back. `ytui doctor`
shows the reason. See [resolver.md](resolver.md).

## "resolver: bad token"

The client and server tokens differ. Regenerate with
`ytui-resolver --gen-token` and point `resolver_token_file` at the result.

## Mouse does nothing

The terminal has to forward mouse events. Windows Terminal and most Linux
terminals do; in tmux add `set -g mouse on` and restart ytui.

## Media keys do nothing

`ytui doctor` prints the media session it selected. On Linux the keys go to the
focused player through MPRIS, so check `playerctl -l` and that
`DBUS_SESSION_BUS_ADDRESS` is set (it is not under plain `ssh` or `sudo -i`).
On Windows make sure `media_keys` is not `false` and that another player is
not holding the session; the volume flyout should list ytui while a track is
loaded. On macOS the keys follow the current Now Playing app, so quit Music if
it keeps claiming them, and make sure ytui runs in a GUI login session (over
plain SSH there is no WindowServer to register with). Under WSL, the native
Windows build should serve the session (`which ytui.exe`, or
`YTUI_MEDIA_BRIDGE=/mnt/c/.../ytui.exe`); without interop, ytui uses WSLg's X
server, so check that `DISPLAY` is set. `ytui doctor` names the backend that
actually registered.

## A long title does not scroll when the terminal is in the background

A long title on the selected row scrolls to show the rest. It stops while the
terminal is in the background, which needs focus reporting: terminals that do
not implement it never report a lost focus, so the title simply keeps
scrolling. Under tmux or screen, enable it with `set -g focus-events on`.

## Windows

- `ytui` is not recognized after installing: the installer added
  `%USERPROFILE%\.local\bin` to the user `PATH`, which only new terminals pick
  up. Open a new Windows Terminal or PowerShell window, or run the full path
  `%USERPROFILE%\.local\bin\ytui.exe`.
- No sound: `ytui doctor` prints the WASAPI device and sample rate it opened.
  Check Settings → System → Sound that the intended output is the default.
  ytui moves to a new default while it is running; a build older than 0.4.7
  needs a restart after switching. WASAPI shared mode plays at the device mix
  rate, usually 48 kHz, so 44.1 kHz tracks are resampled (see
  [audio.md](audio.md)).
- Garbled box drawing or no mouse: use Windows Terminal instead of legacy
  `conhost`.
- `ytui update` cannot replace `ytui.exe`: close the player first. The updater
  renames the running binary, but antivirus can briefly hold the file open.

## Tracks sound muffled

They only had 48 kbps HE-AAC (`itag` 139). Symphonia decodes the AAC core but
not the SBR band, so the highs are missing. ytui prefers AAC-LC when YouTube
offers it — see [audio.md](audio.md).
