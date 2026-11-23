# Web player

`ytui-resolver --web` turns the resolver into a small web player for a
phone: the same server, the same token, and the listings and audio the
terminal player already uses. Off by default, so an existing deployment is
unchanged.

```bash
ytui-resolver --bind 0.0.0.0:8787 --allow-private --web
```

Open `http://<resolver>:8787/` on the phone. With a token configured, the
page asks for it once and keeps it in an HttpOnly cookie; on a trusted
LAN/tailnet (`--allow-private`) no login appears at all. Search and playback
work without cookies; Liked Music needs the server's `--cookies`.

## Routes

| route | what it does |
|---|---|
| `GET /` | the player page, one file embedded in the binary |
| `GET /v1/session` | auth state, whether the server has cookies |
| `POST /v1/login` | `{"token":"..."}` -> session cookie |
| `GET /v1/search?q=` | yt-dlp search, `limit` results (default 30, max 100) |
| `GET /v1/liked` | Liked Music, cached for 15 minutes |
| `GET /audio/<id>` | Range proxy to googlevideo |

The audio is proxied, not linked: googlevideo URLs can be bound to the
requesting IP, so a URL resolved on the server can 403 when the phone fetches
it directly. The proxy forwards the phone's Range headers, reuses the resolve
cache (a mid-track seek on a cached video answers in milliseconds) and takes
a resolve slot only for a cold resolve, never while streaming. When every
candidate refuses, it re-resolves once with the cache bypassed, like the
terminal player does.

The queue lives in the phone's browser storage. Nothing about the web player
touches the terminal player's state.

## Tailscale and TLS

The server has no TLS, like the rest of the resolver. On a tailnet, let
Tailscale provide both the certificate and the name:

```bash
# on the box running the resolver
tailscale serve --bg 8787
```

The phone then opens `https://<machine>.<tailnet>.ts.net/` — with a real
certificate, so iOS is happy about media playback and the home-screen app,
and it works away from home over the same tailnet. On plain LAN Wi-Fi the
`http://` address works as-is. Add to the home screen from Safari's share
sheet to run it full-screen.

## Notes

- The token, `--allow-private`, rate limits and the resolve cache all apply
  as usual; `/audio` gets a wider rate bucket, because range requests are
  how `<audio>` seeks and buffers.
- Lock-screen controls come from the Media Session API: the page sets title,
  artist and the YouTube thumbnail per track.
- The player uses the current track's cover as a dimmed backdrop that fades
  out to the right; it keeps the artwork's aspect at the left edge, so square
  covers are not cropped or pushed right. Title, elapsed/total timers, seek
  bar and transport are centered. On a desktop browser the transport row also
  carries mute, a volume slider and fullscreen (phones use their hardware
  buttons). The volume level is remembered in browser storage, like the
  queue.
- Next/previous keep the visible list in step: the new track scrolls to the
  first row after next, and to the second after previous so the track that
  just played stays in view.
- Shuffle and repeat (queue, then one track) sit at the edges of the transport
  row; repeat all wraps at the end of the queue.
- Safari can report a doubled duration for fragmented HE-AAC. The listing's
  length is authoritative, media time is mapped by measuring how fast the
  clock actually moves, and the page finishes the track itself instead of
  waiting for the browser's silent tail. The lock screen gets the corrected
  length through `setPositionState`, so its scrubber is not doubled either.
- Lock-screen artwork is offered at three sizes and set again once playback
  starts, since iOS can drop metadata set too early.
- Double-clicking a row in Search or Liked plays it, just like its play
  button.
- Install it as a service like any other invocation:
  `ytui-resolver --bind 0.0.0.0:8787 --allow-private --web --install-service`.
- One export is one client still: with `--cookies` on the server, the
  resolver is the cookie client, exactly as [resolver.md](resolver.md)
  describes for resolving.
