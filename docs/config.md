# Configuration

`~/.config/ytui/config.json` (on Windows
`%USERPROFILE%\.config\ytui\config.json`) is created on the first run or by
the installer. A copy of the defaults is in `config.example.json` in the repo.

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
| `resolver_url` | `""` | remote resolver base URL; empty = resolve locally |
| `resolver_token` | `""` | bearer token (prefer the file) |
| `resolver_token_file` | `""` | token file; empty = `~/.config/ytui/resolver.token` when it exists |
| `resolver_timeout_secs` | `20` | how long to wait for the resolver |
| `resolver_fallback` | `true` | use local yt-dlp when the resolver fails |
| `media_keys` | `true` | register with the OS media session for media keys |

## Files on disk

- `~/.config/ytui/config.json` - config
- `~/.config/ytui/cookies.txt` - imported cookies, mode `600`
- `~/.config/ytui/account.json` - channel name and id
- `~/.config/ytui/cache/` - last listing per tab, ignored after logout
- `~/.config/ytui/resolver.token` - token for the remote resolver, mode `600`
- `~/.local/share/ytui/history.json` - play history

On Windows the same layout lives under `%USERPROFILE%`: the config and cookies
are in `%USERPROFILE%\.config\ytui\`, history in
`%USERPROFILE%\.local\share\ytui\`, and the binary is
`%USERPROFILE%\.local\bin\ytui.exe`.
