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
        };
        f.fetch(0)?;
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
        true
    }
    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}
