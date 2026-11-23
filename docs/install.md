# Installation details

What the installers do, manual builds, platform notes and updates. For the
quick path see the [README](../README.md); for the optional resolver
server see [resolver.md](resolver.md).

Both installers are safe to rerun: they keep an existing config and update
the binary in place. Both try the release asset for the platform first and
verify it against the published `SHA256SUMS`. A platform with no asset, a
download that does not verify, or a binary that cannot run falls back to a
source build. `install.sh --from-source` (or `YTUI_FROM_SOURCE=1`) and
`install.ps1 -FromSource` skip the download.

## What the installers do

### Linux, WSL and macOS

For a release install, `install.sh` downloads the binary and puts
`~/.local/bin/ytui` in place, also installing `yt-dlp`, `deno` and the ALSA
runtime when they are missing — the binary links ALSA dynamically, so the
runtime is needed even when nothing is built.

A source build (the fallback above, or `--from-source`) needs a C linker, the
ALSA headers (`libasound2-dev`, `alsa-lib` or `alsa-lib-devel`) and a current
Rust, installed via rustup when the system one is too old; the script installs
them when it can, and prints the package names when it cannot. On macOS it
checks for the Xcode command line tools.

On macOS the launcher runs the binary from a minimal agent bundle at
`~/.local/lib/ytui/ytui.app` instead (see [macOS](#macos)).

### Windows

`install.ps1` downloads `ytui.exe` from the latest release, installs the
standalone `yt-dlp.exe` (so no Python is needed) and unpacks Deno into
`%USERPROFILE%\.deno\bin`, then copies the binary to
`%USERPROFILE%\.local\bin` and adds that directory to the user `PATH`. A
release install needs neither Rust nor a linker.

A source build (`.\install.ps1 -FromSource`) needs the MSVC linker: install
"Visual Studio Build Tools" with the "Desktop development with C++" workload
if you do not have Visual Studio.

From cmd.exe, run `install.cmd` instead; it calls the same script with a
process-local execution policy. If PowerShell refuses to run `install.ps1`,
that wrapper (or `powershell -ExecutionPolicy Bypass -File .\install.ps1`) is
the way around it.

A source build writes the checkout path to
`%USERPROFILE%\.config\ytui\source`; a release install removes that file, so
`ytui update` knows to download a release instead of rebuilding.

### Resolver discovery at install time

When no resolver is configured, the installer scans the local network for up
to 10 seconds on a fresh install and saves a tokenless one if it finds it.
Reruns skip that scan and only fetch `SHA256SUMS` when the installed binary is
already the latest release, so reinstalling is quick. Set
`YTUI_SKIP_DISCOVER=1` (or pass `-SkipDiscover` on Windows) to skip the scan,
or `YTUI_DISCOVER=1` (or `-Discover` on Windows) to run it again. See
[resolver.md](resolver.md#automatic-discovery).

## Manual build

Linux and macOS:

```bash
cargo build --release
mkdir -p ~/.local/bin
cp target/release/ytui ~/.local/bin/ytui
```

Windows:

```powershell
cargo build --release
New-Item -ItemType Directory -Force -Path "$env:USERPROFILE\.local\bin"
Copy-Item target\release\ytui.exe "$env:USERPROFILE\.local\bin\"
```

On macOS prefer `./install.sh`: it also creates the app bundle described in
[macOS](#macos). A binary copied out of the bundle works, but the system shows
it as an unnamed process with a generic icon.

## macOS

ytui publishes the current track to the system Now Playing panel, so the media
keys, Control Center, the Touch Bar and headphone buttons control it like any
other player. Title, channel, artwork and progress come from the track ytui
is playing; play/pause, seek, next and previous work in both directions. No
Dock icon or menu bar item appears.

The macOS build runs from a minimal agent bundle, `~/.local/lib/ytui/ytui.app`
(`LSUIElement`, ad-hoc signed as `dev.tacamihail.ytui`). The bundle is what
gives the process its name and YouTube Music-style icon in Now Playing, Control
Center, the Touch Bar and the app lists; the signature gives it a stable
identity that persists across launches. `install.sh` creates the bundle and
registers it with LaunchServices, and `ytui update` re-signs it after replacing
the binary. If the Touch Bar keeps showing a cached icon, restart its agent
(it relaunches the next time media plays):

```bash
killall NowPlayingTouchUI
```

If the keys still go to Music.app, allow the terminal that launched ytui under
**System Settings → Privacy & Security → Media & Apple Music**.

## WSL

WSL has no sound card, only a PulseAudio socket. ALSA's `default` device still
points at card 0, so ytui asks for the `pulse` PCM first and only falls back to
`default` when that fails. You just need the bridge:

```bash
sudo apt-get install -y libasound2-plugins
```

No `~/.asoundrc` edit is needed. `ytui doctor` prints the PCM it picked, for
example `✔ audio output: pulse @ 44100 Hz stereo`.

If you can't use sudo, `install.sh` extracts the package into `~/.local/alsa`
when apt fails and points the launcher at it with a private ALSA config. Your
`~/.asoundrc` is not touched. The manual equivalent is:

```bash
cd /tmp
apt-get download libasound2-plugins
dpkg -x libasound2-plugins_*.deb ~/.local/alsa
```

Then rerun `./install.sh` so it writes `~/.config/ytui/asound.conf` for the
launcher.

WSL also cannot discover a resolver on your LAN: it only sees the WSL virtual
network. Run the player natively on Windows if you want discovery, or point it
at the resolver explicitly with
`ytui discover --url http://<host>:8787 --save`. Tailnet peers are still probed
when the Windows `tailscale.exe` is reachable through WSL's interop `PATH`.

Media keys in WSL come from the native Windows build: put `ytui.exe` on
`PATH` (the Windows install does this) and the WSL build will start it as a
hidden media-session bridge. If WSL interop is disabled
(`/proc/sys/fs/binfmt_misc/WSLInterop` missing), ytui grabs the media keys
from WSLg's X server instead. See [usage.md](usage.md#media-keys).

## Updates

```bash
ytui update          # install the newer version from github
ytui update --check  # only report whether one exists
```

A release install downloads the asset for the platform. A source install pulls
and rebuilds the checkout. In both cases the version is the `[package] version`
in `Cargo.toml`; commits that do not change it are not installed. `ytui update`
refuses to touch a checkout with uncommitted changes or local commits. It
updates the checkout the installer ran from; set `YTUI_SRC` to point elsewhere.
On macOS the updater replaces the binary inside the `ytui.app` bundle and
re-signs it, so Now Playing keeps its name and icon. Inside the player, `?`
checks github and `u` installs a newer version; a clickable `↑ 0.4.0` chip
appears when one is waiting.

On Windows, close the player before `ytui update`: the updater renames the
running binary, but antivirus can briefly hold the file open.

## Uninstall

Linux, WSL and macOS:

```bash
rm ~/.local/bin/ytui ~/.local/bin/ytui.bin
rm -rf ~/.config/ytui ~/.local/share/ytui
```

Windows:

```powershell
Remove-Item "$env:USERPROFILE\.local\bin\ytui.exe"
Remove-Item -Recurse -Force "$env:USERPROFILE\.config\ytui", "$env:USERPROFILE\.local\share\ytui"
```
