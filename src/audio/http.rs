//! HTTP Range reader used as a symphonia `MediaSource`.
//!
//! YouTube serves audio as a plain file on `*.googlevideo.com`; the URL already
//! carries a signature and expiry, so no cookies are needed for playback. We
//! never download the whole file: one chunk is kept in RAM and re-fetched with
//! a `Range` request whenever the decoder seeks outside it. That keeps the
//! player's memory flat regardless of track length (a 2 h mix costs the same
//! as a 3 min song).

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use symphonia::core::io::MediaSource;

use crate::ytdl::UA;

pub struct HttpFile {
    url: String,
    pos: u64,
    len: Option<u64>,
    cache_start: u64,
    cache: Vec<u8>,
    chunk: usize,
    /// Fragmented MP4 (moof/sidx) is not worth random access: symphonia scans
    /// every top-level atom when the source is seekable, which turns a 6 GB,
    /// 108 h mix into tens of thousands of range requests before the first
    /// sample. Large fragmented streams are therefore reported as unseekable,
    /// which makes the demuxer stop at the first moof. Our own `seek` below
    /// still works for the small ones.
    seekable: bool,
}

impl HttpFile {
    /// Opens the stream and pre-fetches the first chunk (also learns the size).
    pub fn open(url: &str, chunk_bytes: usize) -> io::Result<Self> {
        let mut f = Self {
            url: url.to_string(),
            pos: 0,
            len: None,
            cache_start: 0,
            cache: Vec::new(),
            chunk: chunk_bytes.max(64 * 1024),
            seekable: true,
        };
        f.fetch(0)?;
        // Small fragmented files are cheap to scan (a few dozen fragments), so
        // keep random access there; huge ones must not be scanned at all.
        const FRAGMENTED_SEEK_LIMIT: u64 = 16 * 1024 * 1024;
        let fragmented = looks_fragmented(&f.cache);
        f.seekable = !fragmented || f.len.map(|l| l <= FRAGMENTED_SEEK_LIMIT).unwrap_or(false);
        Ok(f)
    }

    fn fetch(&mut self, start: u64) -> io::Result<()> {
        let end = start.saturating_add(self.chunk as u64 - 1);
        let mut last = String::from("unknown error");
        for attempt in 0..3u32 {
            let res = ureq::get(&self.url)
                .set("Range", &format!("bytes={start}-{end}"))
                .set("User-Agent", UA)
                .timeout(Duration::from_secs(20))
                .call();
            match res {
                Ok(resp) => {
                    if self.len.is_none() {
                        if let Some(cr) = resp.header("Content-Range") {
                            self.len = cr.rsplit('/').next().and_then(|s| s.trim().parse().ok());
                        } else if let Some(cl) = resp.header("Content-Length") {
                            // Server ignored Range and sent the whole body.
                            if start == 0 {
                                self.len = cl.trim().parse().ok();
                            }
                        }
                    }
                    let mut buf = Vec::with_capacity(self.chunk);
                    resp.into_reader().read_to_end(&mut buf)?;
                    if buf.is_empty() {
                        return Err(io::Error::new(
                            io::ErrorKind::UnexpectedEof,
                            "empty range response",
                        ));
                    }
                    self.cache_start = start;
                    self.cache = buf;
                    return Ok(());
                }
                Err(e) => {
                    last = e.to_string();
                    std::thread::sleep(Duration::from_millis(200 * u64::from(attempt + 1)));
                }
            }
        }
        Err(io::Error::new(
            io::ErrorKind::Other,
            format!("range fetch failed: {last}"),
        ))
    }
}

impl Read for HttpFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let in_cache =
            self.pos >= self.cache_start && self.pos < self.cache_start + self.cache.len() as u64;
        if !in_cache {
            self.fetch(self.pos)?;
        }
        let off = (self.pos - self.cache_start) as usize;
        let n = out.len().min(self.cache.len().saturating_sub(off));
        if n == 0 {
            return Ok(0);
        }
        out[..n].copy_from_slice(&self.cache[off..off + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => match self.len {
                Some(l) => l as i64 + d,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "stream length unknown",
                    ))
                }
            },
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HttpFile {
    fn is_seekable(&self) -> bool {
        self.seekable
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}

/// True when the first chunk contains a `moof` or `sidx` box before any
/// `mdat`, i.e. the file is a fragmented MP4 (YouTube uses these for very long
/// uploads). Such files must not be opened in seekable mode: symphonia then
/// walks every top-level atom to look for `moov`, which is O(fragments) and
/// effectively hangs on multi-hour mixes.
fn looks_fragmented(head: &[u8]) -> bool {
    let mut off = 0usize;
    while off + 8 <= head.len() {
        let size32 = u32::from_be_bytes([head[off], head[off + 1], head[off + 2], head[off + 3]]);
        let typ = &head[off + 4..off + 8];
        if typ == b"moof" || typ == b"sidx" {
            return true;
        }
        if typ == b"mdat" {
            return false;
        }
        let size = match size32 {
            0 => return false, // box extends to the end of the file
            1 => {
                if off + 16 > head.len() {
                    return false;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&head[off + 8..off + 16]);
                u64::from_be_bytes(b)
            }
            n => u64::from(n),
        };
        if size < 8 {
            return false;
        }
        match (off as u64).checked_add(size) {
            Some(next) if next <= head.len() as u64 => off = next as usize,
            _ => return false, // box continues past the chunk we probed
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(typ: &[u8; 4], payload: usize) -> Vec<u8> {
        let mut v = ((payload + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.resize(payload + 8, 0);
        v
    }

    #[test]
    fn detects_fragmented_mp4() {
        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"moov", 735));
        head.extend(bx(b"sidx", 4000));
        assert!(looks_fragmented(&head));

        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"moov", 735));
        head.extend(bx(b"moof", 900));
        assert!(looks_fragmented(&head));
    }

    #[test]
    fn accepts_plain_mp4() {
        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"moov", 735));
        head.extend(bx(b"mdat", 4000));
        assert!(!looks_fragmented(&head));

        // mdat first (moov at the end): symphonia only needs one seek for it.
        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"mdat", 100_000));
        assert!(!looks_fragmented(&head));

        // Truncated header must not be mistaken for a fragment.
        assert!(!looks_fragmented(b"\0\0\0\x10fty"));
    }
}
