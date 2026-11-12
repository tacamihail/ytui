//! Newline-delimited JSON shared by the WSL side and the Windows bridge.
//!
//! One flat object per line in each direction. Flat keeps the protocol easy to
//! inspect by hand (`ytui.exe --media-bridge` in a terminal) and avoids serde
//! version skew between two independently built binaries.

use super::{Event, NowPlaying, Playback};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
struct UpdateMsg {
    t: String,
    status: String,
    title: String,
    #[serde(default)]
    artist: String,
    #[serde(default)]
    pos: f64,
    #[serde(default)]
    dur: f64,
    #[serde(default)]
    volume: i64,
    #[serde(default)]
    muted: bool,
    #[serde(default)]
    shuffle: bool,
}

#[derive(Serialize, Deserialize)]
struct EventMsg {
    t: String,
    event: String,
    #[serde(default)]
    v: f64,
}

#[derive(Serialize, Deserialize)]
#[allow(dead_code)] // only parsed on the WSL side
struct ReadyMsg {
    t: String,
}

/// First line from a healthy Windows bridge. It proves that the `ytui.exe` on
/// PATH is new enough to know `--media-bridge`; an old install exits instead,
/// and the WSL side falls back to the X11 grab.
#[allow(dead_code)] // Windows bridge writes it; the WSL side only reads it.
pub fn encode_ready() -> String {
    r#"{"t":"ready"}"#.to_string()
}

#[allow(dead_code)] // Linux bridge reads it; the Windows side only writes it.
pub fn is_ready(line: &str) -> bool {
    serde_json::from_str::<ReadyMsg>(line)
        .map(|msg| msg.t == "ready")
        .unwrap_or(false)
}

#[allow(dead_code)] // Linux side only; the Windows side uses encode_event instead.
pub fn encode_update(now: &NowPlaying) -> String {
    let status = match now.playback {
        Playback::Stopped => "stopped",
        Playback::Playing => "playing",
        Playback::Paused => "paused",
    };
    let msg = UpdateMsg {
        t: "u".to_string(),
        status: status.to_string(),
        title: now.title.clone(),
        artist: now.artist.clone(),
        pos: finite(now.position),
        dur: finite(now.duration),
        volume: now.volume,
        muted: now.muted,
        shuffle: now.shuffle,
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

#[allow(dead_code)] // Windows side only; the Linux side uses encode_update instead.
pub fn parse_update(line: &str) -> Option<NowPlaying> {
    let msg: UpdateMsg = serde_json::from_str(line).ok()?;
    if msg.t != "u" {
        return None;
    }
    let playback = match msg.status.as_str() {
        "playing" => Playback::Playing,
        "paused" => Playback::Paused,
        _ => Playback::Stopped,
    };
    Some(NowPlaying {
        playback,
        title: msg.title,
        artist: msg.artist,
        duration: finite(msg.dur),
        position: finite(msg.pos),
        volume: msg.volume.clamp(0, 100),
        muted: msg.muted,
        shuffle: msg.shuffle,
    })
}

#[allow(dead_code)] // Windows side only; the Linux side uses parse_event instead.
pub fn encode_event(event: &Event) -> String {
    let (name, value) = match *event {
        Event::Play => ("play", 0.0),
        Event::Pause => ("pause", 0.0),
        Event::Toggle => ("toggle", 0.0),
        Event::Next => ("next", 0.0),
        Event::Prev => ("prev", 0.0),
        Event::Stop => ("stop", 0.0),
        Event::Mute => ("mute", 0.0),
        Event::Seek(seconds) => ("seek", seconds),
        Event::SeekAbs(seconds) => ("seek_abs", seconds),
        Event::Volume(volume) => ("volume", volume),
        Event::Shuffle(on) => ("shuffle", if on { 1.0 } else { 0.0 }),
        Event::Quit => ("quit", 0.0),
    };
    let msg = EventMsg {
        t: "e".to_string(),
        event: name.to_string(),
        v: finite(value),
    };
    serde_json::to_string(&msg).unwrap_or_default()
}

#[allow(dead_code)] // Linux side only; the Windows side uses parse_update instead.
pub fn parse_event(line: &str) -> Option<Event> {
    let msg: EventMsg = serde_json::from_str(line).ok()?;
    if msg.t != "e" {
        return None;
    }
    Some(match msg.event.as_str() {
        "play" => Event::Play,
        "pause" => Event::Pause,
        "toggle" => Event::Toggle,
        "next" => Event::Next,
        "prev" => Event::Prev,
        "stop" => Event::Stop,
        "mute" => Event::Mute,
        "seek" => Event::Seek(finite(msg.v)),
        "seek_abs" => Event::SeekAbs(finite(msg.v)),
        "volume" => Event::Volume(msg.v.clamp(0.0, 1.0)),
        "shuffle" => Event::Shuffle(msg.v != 0.0),
        "quit" => Event::Quit,
        _ => return None,
    })
}

fn finite(value: f64) -> f64 {
    if value.is_finite() {
        value
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_round_trips() {
        let now = NowPlaying {
            playback: Playback::Paused,
            title: "Song \"one\"".into(),
            artist: "Artist".into(),
            duration: 200.5,
            position: 12.25,
            volume: 35,
            muted: true,
            shuffle: true,
        };
        let parsed = parse_update(&encode_update(&now)).unwrap();
        assert_eq!(parsed, now);
    }

    #[test]
    fn transport_round_trips() {
        for event in [
            Event::Play,
            Event::Pause,
            Event::Toggle,
            Event::Next,
            Event::Prev,
            Event::Stop,
            Event::Mute,
            Event::Seek(-10.0),
            Event::SeekAbs(42.0),
            Event::Volume(0.25),
            Event::Shuffle(false),
            Event::Quit,
        ] {
            assert_eq!(parse_event(&encode_event(&event)), Some(event));
        }
    }

    #[test]
    fn the_ready_line_is_distinct_from_events() {
        assert!(is_ready(&encode_ready()));
        assert!(!is_ready(&encode_event(&Event::Toggle)));
        assert!(!is_ready("not json"));
    }

    #[test]
    fn garbage_is_ignored() {
        assert!(parse_update("not json").is_none());
        assert!(parse_update(r#"{"t":"e","event":"next","v":0}"#).is_none());
        assert!(parse_event(r#"{"t":"u","status":"playing"}"#).is_none());
    }
}
