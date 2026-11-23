# Account and cookies

Liked Music, playlists and subscriptions need cookies from a browser that is
signed in to YouTube. There is no OAuth app and no API key: YouTube disabled
OAuth for yt-dlp, so cookies are the only way in. Search and playback work
without an account.

`ytui auth` imports cookies and checks them against YouTube, saves the channel
in `~/.config/ytui/account.json` and copies them to
`~/.config/ytui/cookies.txt` with mode `600`. The export has to include the
HttpOnly cookies: without them the file is signed out, and `ytui auth` refuses
it rather than overwriting a working `~/.config/ytui/cookies.txt`.

## Exporting cookies that keep working

YouTube rotates the account cookies on every open YouTube tab, as a security
measure. An export taken from a normal browser window goes stale within hours
or days, and exporting again from that same window keeps it stale. The fix is
to export from a session that is never opened again:

1. install "Get cookies.txt LOCALLY" and allow it in private windows
   (`chrome://extensions` → Details → *Allow in Incognito*)
2. open a **new private/incognito window** and sign in to
   <https://music.youtube.com>
3. in that same tab, open <https://www.youtube.com/robots.txt>; keep it the
   only private tab
4. export the `youtube.com` cookies to `cookies.txt`
5. **close the private window** and never reopen it

Nothing rotates that session after step 5, so the file keeps working for weeks
or months instead of expiring. This is the same advice as the "Exporting
YouTube cookies" section of the yt-dlp wiki.

## One export is one client

Installing the same `cookies.txt` on a second machine, or importing it inside
WSL, gives that copy the same session; the two then rotate the token
independently and Google signs both out. If you want ytui on two machines,
sign in to each one in its own private window and export separately. A
resolver started with `--cookies` is a third client; leave it anonymous
(playback falls back to local yt-dlp for age-restricted tracks, see
[resolver.md](resolver.md)).

## The cookie jar lock

ytui serializes its own yt-dlp runs around the `--cookies` file. yt-dlp saves
the jar when it exits, which is how the rotating `__Secure-1PSIDTS` stays
current. A listing, a background prefetch and a manual resolve used to be able
to write the same jar at once; the last save could put a stale token back, and
the next request would sign the account out. The runs now share a
`cookies.txt.lock` file next to it, across the player, `ytui doctor` and
`ytui auth`. A second machine with its own copy of the same session cannot
share that lock — see above.

## Subscriptions

The subscriptions tab used to be fetched with a raw `Cookie:` header. That read
the stored `__Secure-1PSIDTS` but threw away the rotated value YouTube sent
back, so the jar aged faster than the clock and could be rejected as a replay.
Subscriptions now go through the same yt-dlp listing as the other tabs, which
saves the rotation. They also work with `--browser` cookies now.

## Reading cookies from the browser

`ytui auth --browser <name>` skips the export: yt-dlp reads the browser's
cookie database on every run, so there is nothing in a file to go stale. The
browser has to stay signed in to YouTube. One limit:

- on Linux, Chrome-family browsers encrypt their cookies with the system
  keyring. It works only if the keyring is unlocked and yt-dlp can decrypt
  them, otherwise `ytui auth` reports that it could not verify the account.
