# Audio pipeline

How ytui turns a YouTube video into sound: stream selection, sample rates,
buffering and the handover between tracks. For what the pipeline costs in RAM,
see [memory.md](memory.md).

## Stream selection

ytui only downloads an audio stream, so the video resolution on YouTube does
not matter. It asks for AAC (`itag` 141, then 140, then 139) and probes each
URL with a 1 KiB request, because YouTube sometimes returns 403 for one of
them. The same itag can appear more than once, once per YouTube player
client: their URLs are not equally usable, so the first copy that answers
wins. The `visionos` client goes first because its URLs need no PO token.

Opus (`itag` 251) would be a bit better than 128 kbps AAC, but `symphonia` has
no Opus decoder, so those uploads are skipped with a message.

## Sample rate

The output stream is opened at the track's own sample rate. YouTube AAC is
44.1 kHz, so on a 44.1 kHz device nothing is resampled. If the device refuses
that rate, ytui falls back to 48 kHz and resamples. Windows WASAPI shared mode
only accepts the device mix format, which is usually 48 kHz, so there the
44.1 kHz tracks are resampled by default.

## Output device (Windows)

On Windows the player follows the system default output device, checked every
two seconds: connecting Bluetooth headphones mid-song reopens the stream on
them and carries the unplayed seconds over, instead of leaving the music on
the speakers the stream was opened on.

## Buffering

The audio is pulled in 256 KiB HTTP ranges by a background thread that keeps
about a minute of encoded sound ready ahead of the decoder. The decoded ring
is only two seconds, so a range request that stalls for a few seconds is no
longer an audible hole in the middle of a track; the music plays on while the
request catches up. The ranges reuse one keep-alive connection instead of
opening a new TLS session every sixteen seconds.

## Track handover

Working out a stream URL takes `yt-dlp` about four seconds locally and up to
twenty on a cold resolver. The next track is requested once the current one is
within twice the measured resolve time (never less than twenty seconds), and
only its URL is kept (a few hundred bytes, no audio), so the handover costs
about 0.2 s without holding links for songs that are still far away. A
resolve that hangs is killed after 30 seconds, and a failed one is retried
once: otherwise a transient error would leave the next track to be resolved at
the handover, which is heard as a pause.

Resolving runs at `nice 10`, so it does not compete with the decoder. A track
you choose yourself is resolved immediately and never waits behind the
prefetch. On a machine where audio still crackles during playback,
`YTUI_PREFETCH=0 ytui` disables background resolution.

## Tail silence

When a track ends in seven seconds or more of silence, ytui moves to the next
one at the start of that silence instead of playing it out. Finding it costs
no audio buffer: a short-lived second decoder reads the last twelve seconds
once, keeps one RMS number per 100 ms (under half a kilobyte) and throws the
samples away. The verdict needs both an absolute floor (−45 dBFS) and a
relative gate, so a quiet song keeps its ending.

## HE-AAC muffling

Tracks that sound muffled only had 48 kbps HE-AAC (`itag` 139). Symphonia
decodes the AAC core but not the SBR band, so the highs are missing. ytui
prefers AAC-LC when YouTube offers it, and tries every player client's URL for
that itag before accepting 139: the `tv` client's 128 kbps link is the one that
usually answers with 403.
