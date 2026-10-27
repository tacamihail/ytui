//! HTTP Range reader used as a symphonia `MediaSource`.
//!
//! YouTube serves audio as a plain file on `*.googlevideo.com`; the URL already
//! carries a signature and expiry, so no cookies are needed for playback. We
//! never download the whole file: one chunk is kept in RAM and re-fetched with
//! a `Range` request whenever the decoder seeks outside it. That keeps the
//! player's memory flat regardless of track length (a 2 h mix costs the same
//! as a 3 min song).
//!
//! Very long uploads are fragmented MP4 (`moov` + `sidx` + one `moof`/`mdat`
//! pair per ~10 s). symphonia scans every top-level atom when a source is
//! seekable, which is unusable there, so we parse the `sidx` ourselves and
//! expose a small `WindowFile` that starts at any chosen fragment: seeking is
//! one HTTP request instead of walking thousands of fragments.

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
    /// Offset of the first `moof`/`sidx` box in the file (fragmented MP4), or
    /// `None` for a plain faststart/non-faststart MP4.
    marker: Option<u64>,
    /// False for large fragmented streams: symphonia must not scan them.
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
            marker: None,
            seekable: true,
        };
        f.fetch(0)?;
        f.marker = fragmented_marker(&f.cache);
        // Small fragmented files are cheap to scan (a few dozen fragments), so
        // keep symphonia's random access there; huge ones must not be scanned.
        const FRAGMENTED_SEEK_LIMIT: u64 = 16 * 1024 * 1024;
        f.seekable =
            f.marker.is_none() || f.len.map(|l| l <= FRAGMENTED_SEEK_LIMIT).unwrap_or(false);
        Ok(f)
    }

    /// Opens the stream positioned at `start` (used for the fragment window).
    pub fn open_at(url: &str, chunk_bytes: usize, start: u64) -> io::Result<Self> {
        let mut f = Self {
            url: url.to_string(),
            pos: start,
            len: None,
            cache_start: 0,
            cache: Vec::new(),
            chunk: chunk_bytes.max(64 * 1024),
            marker: None,
            seekable: false,
        };
        f.fetch(start)?;
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

    /// Reads an arbitrary range without disturbing the stream position.
    fn read_range(&mut self, start: u64, len: usize) -> io::Result<Vec<u8>> {
        let saved = self.pos;
        self.seek(SeekFrom::Start(start))?;
        let mut buf = vec![0u8; len];
        self.read_exact(&mut buf)?;
        self.pos = saved;
        Ok(buf)
    }

    /// Parses the segment index of a fragmented stream, if there is one.
    pub fn frag_index(&mut self) -> io::Result<Option<FragIndex>> {
        let Some(marker) = self.marker else {
            return Ok(None);
        };
        let head = self.read_range(marker, 8)?;
        if &head[4..8] != b"sidx" {
            return Ok(None);
        }
        let size32 = u32::from_be_bytes([head[0], head[1], head[2], head[3]]);
        let (size, hdr) = if size32 == 1 {
            let b = self.read_range(marker, 16)?;
            let mut s = [0u8; 8];
            s.copy_from_slice(&b[8..16]);
            (u64::from_be_bytes(s), 16)
        } else if size32 == 0 {
            return Ok(None);
        } else {
            (u64::from(size32), 8)
        };
        // Sanity: a segment index is never more than a few MB.
        if size < hdr as u64 || size > 8 * 1024 * 1024 {
            return Ok(None);
        }
        let box_bytes = self.read_range(marker, size as usize)?;
        Ok(parse_sidx(&box_bytes, marker))
    }

    /// Bytes from the start of the file up to the first fragment. Together
    /// with a chosen fragment this is all a `WindowFile` needs.
    pub fn fragment_prefix(&mut self) -> io::Result<Option<Vec<u8>>> {
        let Some(marker) = self.marker else {
            return Ok(None);
        };
        if marker > 64 * 1024 * 1024 {
            return Ok(None);
        }
        Ok(Some(self.read_range(0, marker as usize)?))
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

/// A stream that presents `prefix` followed by the real file from `start`.
///
/// The fragment's `moof` therefore sits at virtual offset `prefix.len()`, and
/// because these streams use `default-base-is-moof` every sample offset inside
/// it is relative to that position — the demuxer's absolute seeks stay inside
/// our virtual address space and map back to the real file here.
pub struct WindowFile {
    prefix: Vec<u8>,
    inner: HttpFile,
    window_start: u64,
    pos: u64,
    len: u64,
    need_seek: bool,
}

impl WindowFile {
    pub fn open(
        url: &str,
        chunk_bytes: usize,
        prefix: Vec<u8>,
        window_start: u64,
    ) -> io::Result<Self> {
        let inner = HttpFile::open_at(url, chunk_bytes, window_start)?;
        let real_len = inner.byte_len().unwrap_or(window_start);
        let len = prefix.len() as u64 + real_len.saturating_sub(window_start);
        Ok(Self {
            prefix,
            inner,
            window_start,
            pos: 0,
            len,
            need_seek: true,
        })
    }
}

impl Read for WindowFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        let prefix_len = self.prefix.len() as u64;
        if self.pos < prefix_len {
            let off = self.pos as usize;
            let n = out.len().min(self.prefix.len() - off);
            out[..n].copy_from_slice(&self.prefix[off..off + n]);
            self.pos += n as u64;
            self.need_seek = true;
            return Ok(n);
        }
        let real = self.window_start + (self.pos - prefix_len);
        if self.need_seek {
            self.inner.seek(SeekFrom::Start(real))?;
            self.need_seek = false;
        }
        let n = self.inner.read(out)?;
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for WindowFile {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.len as i64 + d,
        };
        if target < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "negative seek"));
        }
        self.pos = target as u64;
        self.need_seek = true;
        Ok(self.pos)
    }
}

impl MediaSource for WindowFile {
    fn is_seekable(&self) -> bool {
        // Keep symphonia from rescanning. Our own seek still works, but the
        // player re-opens at a fragment instead of using the demuxer's seek.
        false
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.len)
    }
}

/// Time → byte-offset index built from an MP4 `sidx` box.
#[derive(Debug, Clone)]
pub struct FragIndex {
    timescale: u32,
    /// (start time in timescale units, absolute byte offset of the `moof`).
    entries: Vec<(u64, u64)>,
}

impl FragIndex {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn offset(&self, i: usize) -> u64 {
        self.entries[i].1
    }

    /// Fragment containing `secs`, as (index, actual start time in seconds).
    pub fn find(&self, secs: f64) -> Option<(usize, f64)> {
        if self.entries.is_empty() {
            return None;
        }
        let ticks = (secs.max(0.0) * f64::from(self.timescale)) as u64;
        let idx = match self.entries.binary_search_by_key(&ticks, |e| e.0) {
            Ok(i) => i,
            Err(0) => 0,
            Err(i) => i - 1,
        };
        Some((idx, self.entries[idx].0 as f64 / f64::from(self.timescale)))
    }
}

fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Parses a complete `sidx` box that starts at `box_offset` in the file.
fn parse_sidx(b: &[u8], box_offset: u64) -> Option<FragIndex> {
    if b.len() < 32 || &b[4..8] != b"sidx" {
        return None;
    }
    let version = b[8];
    let timescale = be32(&b[16..20]);
    if timescale == 0 {
        return None;
    }
    let (first_offset, count, cursor) = if version == 0 {
        let fo = be32(&b[24..28]) as u64;
        let c = u16::from_be_bytes([b[30], b[31]]) as usize;
        (fo, c, 32usize)
    } else if version == 1 {
        let mut e = [0u8; 8];
        e.copy_from_slice(&b[28..36]);
        let fo = u64::from_be_bytes(e);
        let c = u16::from_be_bytes([b[38], b[39]]) as usize;
        (fo, c, 40usize)
    } else {
        return None;
    };
    if b.len() < cursor + count * 12 {
        return None;
    }
    let size = u64::from(be32(&b[0..4]));
    let base = box_offset + size + first_offset;
    let mut entries = Vec::with_capacity(count);
    let mut t = 0u64;
    let mut off = base;
    for i in 0..count {
        let at = cursor + i * 12;
        let reference = be32(&b[at..at + 4]);
        let dur = be32(&b[at + 4..at + 8]);
        entries.push((t, off));
        t += u64::from(dur);
        off += u64::from(reference & 0x7FFF_FFFF);
    }
    if entries.is_empty() {
        return None;
    }
    Some(FragIndex { timescale, entries })
}

/// Offset of the first `moof`/`sidx` box when it appears before any `mdat`,
/// i.e. the file is a fragmented MP4 (YouTube uses these for long uploads).
fn fragmented_marker(head: &[u8]) -> Option<u64> {
    let mut off = 0usize;
    while off + 8 <= head.len() {
        let size32 = be32(&head[off..off + 4]);
        let typ = &head[off + 4..off + 8];
        if typ == b"moof" || typ == b"sidx" {
            return Some(off as u64);
        }
        if typ == b"mdat" {
            return None;
        }
        let size = match size32 {
            0 => return None, // box extends to the end of the file
            1 => {
                if off + 16 > head.len() {
                    return None;
                }
                let mut b = [0u8; 8];
                b.copy_from_slice(&head[off + 8..off + 16]);
                u64::from_be_bytes(b)
            }
            n => u64::from(n),
        };
        if size < 8 {
            return None;
        }
        match (off as u64).checked_add(size) {
            Some(next) if next <= head.len() as u64 => off = next as usize,
            _ => return None, // box continues past the chunk we probed
        }
    }
    None
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
        assert_eq!(fragmented_marker(&head), Some(767));

        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"moov", 735));
        head.extend(bx(b"moof", 900));
        assert_eq!(fragmented_marker(&head), Some(767));
    }

    #[test]
    fn accepts_plain_mp4() {
        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"moov", 735));
        head.extend(bx(b"mdat", 4000));
        assert_eq!(fragmented_marker(&head), None);

        // mdat first (moov at the end): symphonia only needs one seek for it.
        let mut head = bx(b"ftyp", 16);
        head.extend(bx(b"mdat", 100_000));
        assert_eq!(fragmented_marker(&head), None);

        // Truncated header must not be mistaken for a fragment.
        assert_eq!(fragmented_marker(b"\0\0\0\x10fty"), None);
    }

    /// Two fragments of 10 s / 1000 bytes each, starting after the box.
    fn sidx_box(entries: &[(u32, u32, u32)]) -> Vec<u8> {
        let payload = 32 + entries.len() * 12;
        let mut v = (payload as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b"sidx");
        v.extend_from_slice(&[0, 0, 0, 0]); // version + flags
        v.extend_from_slice(&1u32.to_be_bytes()); // reference id (track)
        v.extend_from_slice(&44_100u32.to_be_bytes()); // timescale
        v.extend_from_slice(&0u32.to_be_bytes()); // earliest pts
        v.extend_from_slice(&0u32.to_be_bytes()); // first offset
        v.extend_from_slice(&0u16.to_be_bytes()); // reserved
        v.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for &(size, dur, _) in entries {
            v.extend_from_slice(&size.to_be_bytes());
            v.extend_from_slice(&dur.to_be_bytes());
            v.extend_from_slice(&0u32.to_be_bytes());
        }
        v
    }

    #[test]
    fn parses_and_searches_sidx() {
        let entries = [(1000, 441_000, 0), (1000, 441_000, 0), (1000, 441_000, 0)];
        let b = sidx_box(&entries);
        let idx = parse_sidx(&b, 0).expect("sidx");
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.offset(0), b.len() as u64);
        assert_eq!(idx.offset(1), b.len() as u64 + 1000);
        assert_eq!(idx.find(0.0), Some((0, 0.0)));
        assert_eq!(idx.find(9.5), Some((0, 0.0)));
        assert_eq!(idx.find(10.0), Some((1, 10.0)));
        assert_eq!(idx.find(25.0), Some((2, 20.0)));
        assert_eq!(idx.find(999.0), Some((2, 20.0)));
    }
}
