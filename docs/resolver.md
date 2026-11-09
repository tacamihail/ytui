# Remote resolver

`ytui-resolver` is a small HTTP server that turns YouTube watch URLs into AAC
stream candidates. The `ytui` client keeps search, playlists, subscriptions,
cookies and audio playback. Only the `yt-dlp`/deno part moves, which is the
part that costs about 110 MB plus a 300 MB peak for a few seconds per track.

The server is optional. With `resolver_url` empty the client behaves exactly
as before and resolves locally.

## Requirements

- `yt-dlp` and `deno` on the server, the same as for a local install
- Rust 1.83+ (the installer upgrades old system Rust via rustup)
- a LAN, Tailscale or WireGuard path between the client and the server

## Build and install

The resolver is its own package, so a server does not build the TUI, audio
crates or ALSA. The binary is about 600 KB. There are no prebuilt server
binaries yet, so the server needs Rust for either route below. Pick one:

```bash
# no checkout to manage: cargo builds only ytui-resolver and ytui-resolve
cargo install --git https://github.com/tacamihail/ytui ytui-resolver
```

or, from a checkout:

```bash
git clone https://github.com/tacamihail/ytui.git
cd ytui
./install-resolver.sh          # add --token for public/token mode
```

`install-resolver.sh` installs `yt-dlp` and `deno` if they are missing, then
builds only `-p ytui-resolver`. The manual equivalent is:

```bash
cargo build --release -p ytui-resolver
install -Dm755 target/release/ytui-resolver ~/.local/bin/ytui-resolver
```

`cargo install` leaves `yt-dlp` and `deno` to you; the script installs them.

If you want the script's `yt-dlp`/`deno` setup without keeping a checkout:

```bash
curl -fsSLO https://raw.githubusercontent.com/tacamihail/ytui/main/install-resolver.sh
bash install-resolver.sh --from-git
```

`--from-git` runs `cargo install` under the hood, so only the resolver package
is built. Set `YTUI_BRANCH` if you are installing from a different branch.

## Token

Non-loopback binds refuse to start without a token, unless `--allow-private`
is passed to trust private peers.

```bash
ytui-resolver --gen-token
# wrote token to /home/you/.config/ytui/resolver.token
```

The file is mode `600` on Unix. The server reads it automatically, and so does
the client when `resolver_token_file` is empty. On a server that has no
`~/.config/ytui` yet, `--gen-token` creates the directory.

You can also pass `--token` or `--token-file PATH`. Compare tokens in constant
time; there is no token in the URL or in the logs.

## Run

```bash
# loopback only, no token needed
ytui-resolver

# a private network address
ytui-resolver --bind 0.0.0.0:8787 --cookies ~/.config/ytui/cookies.txt

# a specific tailnet address
ytui-resolver --bind 100.64.0.5:8787
```

Options:

| flag | default | meaning |
|---|---|---|
| `--bind` | `127.0.0.1:8787` | listen address |
| `--token` / `--token-file` | auto | bearer token |
| `--gen-token` | - | write a random token and exit |
| `--allow-private` | off | trust LAN/Tailscale peers without a token |
| `--install-service` | - | start at login and exit |
| `--uninstall-service` | - | remove the autostart entry and stop it |
| `--cookies PATH` | none | cookies for age-restricted/members-only tracks |
| `--cache-ttl-secs` | `14400` | how long a resolved URL is reused |
| `--cache-cap` | `512` | cached videos |
| `--max-concurrency` | `2` | simultaneous yt-dlp runs |

The cookie file is optional. Without it the server resolves what YouTube
serves to an anonymous client. With it, the server can use its own account
instead of forwarding the client's cookies per request.

## Tokenless private network

For a LAN or tailnet, `--allow-private` trusts peers in private ranges without
a token:

```bash
ytui-resolver --bind 0.0.0.0:8787 --allow-private
```

The ranges are loopback, RFC1918 (`10/8`, `172.16/12`, `192.168/16`),
link-local, IPv6 ULA/link-local, and Tailscale/CGNAT (`100.64.0.0/10`).
Public peers still need a token; with no token configured they get `401`.

Without `--allow-private` a non-loopback bind still refuses to start unless a
token is configured, which is the safe default. The flag only affects peers
that are already on a private network.

## Public address with a token

Binding a public address is supported with a token:

```bash
ytui-resolver --bind 0.0.0.0:8787 --token-file ~/.config/ytui/resolver.token
```

The server speaks plain HTTP, so terminate TLS in Caddy/nginx and forward to
the resolver on loopback. The client only needs the public URL and the same
token:

```json
"resolver_url": "https://resolver.example.net",
"resolver_token_file": "~/.config/ytui/resolver.token"
```

The automatic scan does not probe public addresses; it is only for discovery
on the local network. A public resolver is configured by hand (or by editing
`config.json` with `ytui` closed) and is never overwritten by `install.sh`.

## Automatic discovery

`install.sh` scans for up to 10 seconds when `resolver_url` is still empty,
and the help overlay does the same with `?` then `s`. From a shell:

```bash
ytui discover --save --timeout 10
```

The scan probes port 8787 on localhost and each private IPv4 `/24` the machine
is attached to. Every probe has a short connect timeout, so a silent host
cannot stretch the scan past its deadline. A tokenless result is preferred and
applied automatically, and the scan stops as soon as one answers; a
token-protected result is only applied when `resolver_token_file` already
points at a token.

Discovery stays provider-neutral on purpose: it does not shell out to
Tailscale, ZeroTier or any other overlay CLI. Those networks are not a local
`/24`, so point ytui at the resolver once instead:

```bash
ytui discover --url http://100.64.0.5:8787 --save
```

The same applies to a resolver on another subnet, over a VPN, or to a WSL
client, which only sees the WSL virtual network rather than the Windows LAN.

## Client config

Leave `resolver_token_file` empty for a tokenless private resolver; point it
at the token file for a public or token-protected one.

```json
"resolver_url": "http://100.64.0.5:8787",
"resolver_token_file": "~/.config/ytui/resolver.token",
"resolver_timeout_secs": 20,
"resolver_fallback": true
```

Environment overrides:

```bash
YTUI_RESOLVER=http://100.64.0.5:8787
YTUI_RESOLVER_TOKEN=...        # or use the token file
```

`ytui doctor` checks the resolver first:

```text
✔ resolver http://100.64.0.5:8787 (v0.4.0, 34 ms)
· resolver yt-dlp 2026.10.26 · deno 2.9.8
✔ yt-dlp 2026.10.26
```

If the resolver is down and `resolver_fallback` is true, doctor reports it and
checks the local yt-dlp/deno path instead. With fallback off, a resolver
failure is a hard error and local yt-dlp is not required for playback.

## Run at login

`--install-service` writes the native autostart entry for this machine, using
the options you passed, and starts it now on Linux and macOS:

```bash
# trusted LAN/tailnet
ytui-resolver --bind 0.0.0.0:8787 --allow-private --install-service

# token mode: --gen-token first; the entry stores only the token file path
ytui-resolver --gen-token
ytui-resolver --bind 0.0.0.0:8787 --install-service
```

| OS | autostart entry |
|---|---|
| Linux | `~/.config/systemd/user/ytui-resolver.service`; `loginctl enable-linger "$USER"` keeps it running without a login session |
| macOS | `~/Library/LaunchAgents/com.ytui.resolver.plist`, logs to `~/.config/ytui/resolver.log` |
| Windows | `ytui-resolver.cmd` in your Startup folder, no admin rights needed |

Rerunning the command replaces the entry. `--uninstall-service` stops and
removes it. Follow the logs with `journalctl --user -u ytui-resolver -f` on
Linux, the log file on macOS, and the console on Windows.

A token passed as `--token` is written to `~/.config/ytui/resolver.token` with
mode `600`, so the unit never contains the secret itself.

`tools/ytui-resolver.service` is still there if you prefer to manage a unit by
hand.

## Protocol v1

```text
GET /v1/health
  -> 200 { "ok": true, "version": "0.4.0",
           "ytdlp": "2026.10.26", "deno": "deno 2.9.8",
           "auth": false }

GET /v1/resolve?url=<watch-url>[&refresh=1]
  Authorization: Bearer <token>
  -> 200 { "v": 1, "id": "...", "title": "...", "channel": "...",
           "duration": 213.0, "ms": 812,
           "candidates": [ { "url": "...", "itag": "141", "abr": 256 } ] }
  -> 400 bad/missing url
  -> 401 bad token
  -> 429 rate limited
  -> 502 yt-dlp failed ({"error": "<friendly text>"})
```

- The server accepts only `youtube.com/watch?v=`, `youtube.com/shorts/`,
  `/live/`, `/embed/` and `youtu.be/` links, extracts the 11-character video
  id and rebuilds a canonical watch URL. Extra query parameters are dropped,
  so nothing from the wire reaches yt-dlp verbatim.
- Candidates are ordered 141 (256 kbps) -> 140 (128 kbps) -> 139 (48 kbps
  HE-AAC), higher bitrate first.
- `auth` says whether this particular peer would need a token for
  `/v1/resolve`. A private peer of a server started with `--allow-private`
  sees `false`.
- The client probes each candidate with a 1 KiB HTTP Range request. That probe
  stays local because googlevideo URLs can be bound to the requesting IP.
- If every candidate is refused, the client calls `/v1/resolve?...&refresh=1`
  once to bypass the server cache and retries.

## Cache and limits

- 4 h TTL, 512 entries. googlevideo links normally stay valid for about 6 h.
- `refresh=1` skips the cache but still stores the new answer.
- At most `--max-concurrency` yt-dlp processes run at once; further requests
  wait in the socket queue.
- 60 requests per minute per peer IP, including cache hits. Health is exempt.

## Security

There is no TLS in the process. That is deliberate: Tailscale and WireGuard
already encrypt and authenticate the path, and a private network is the
intended deployment.

- Bind a private address and keep the port firewalled from the internet.
- On loopback no token is needed. For a LAN/tailnet, either use a token or
  pass `--allow-private`, which trusts only private peer addresses.
- Never expose a tokenless resolver to the internet: public peers are denied,
  but the resolver itself still has no TLS.
- If it must be public, put Caddy/nginx in front for TLS and rate limiting,
  keep the resolver itself on loopback or behind the proxy, and use a token.
- The server sees every video you resolve. Treat its logs and cookie file
  like the YouTube account they belong to.

## Troubleshooting

`401 bad token`: the client token and server token differ. Re-run
`ytui-resolver --gen-token` and point `resolver_token_file` at the file.

`error: package ... requires rustc 1.83 or newer`: Debian and Raspberry Pi
OS ship an old `rustc` (often 1.63). Install rustup and retry:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
source "$HOME/.cargo/env"
cargo install --git https://github.com/tacamihail/ytui ytui-resolver
```

`install-resolver.sh` performs this upgrade automatically when `rustc` is too
old. On a 32-bit ARMv6 board (Pi 1/Zero) rustup may not have a toolchain; build
a prebuilt binary elsewhere or use a newer board.

`scan found nothing`: the resolver is on another subnet, port 8787 is
firewalled, or the client is inside WSL and sees only the WSL virtual network.
Configure `resolver_url` by hand, or run the server on the host/another
machine with a route to the client. Check with `curl http://HOST:8787/v1/health`.

`token required` next to a scan result: that server is not using
`--allow-private`, so it expects the bearer token. Put the token in
`resolver_token_file`, or start the server with `--allow-private` on a trusted
network.

`unreachable (...)`: the client cannot connect. Check `ytui doctor`, the bind
address and the firewall. With fallback on, playback continues locally.

`502 unavailable on youtube`: yt-dlp on the server rejected the video. Check
the server's yt-dlp version and its cookie file, if it has one.

Stale URL/403 loop: the client already retries once with `refresh=1`. If it
keeps happening, the server's and client's public IPs may be far apart and the
links are IP-bound; try running the resolver closer to the client or keep
`resolver_fallback` on.
