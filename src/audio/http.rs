//! HTTP Range reader used as a symphonia `MediaSource`.
//!
//! YouTube serves audio as a plain file on `*.googlevideo.com`; the URL carries
//! its own signature and expiry, so playback needs no cookies. A worker thread
//! downloads fixed-size ranges *ahead* of the decoder, so a range request that
//! takes a few seconds cannot drain the audio ring and leave a hole in the
//! music. Only a bounded window is kept in memory, which stays flat for any
//! track length.
//!
//! Long uploads are fragmented MP4: a `moov` header, a `sidx` index and one
//! `moof`/`mdat` pair every 10 seconds or so. Symphonia walks every atom when
//! a source reports itself seekable, which takes forever on a multi-hour file,
//! so we read the index ourselves and reopen the stream at the right fragment.

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use symphonia::core::io::MediaSource;

use crate::ytdl::UA;

/// Encoded audio kept downloaded ahead of the decoder, in bytes.
///
/// At the 128 kbps YouTube normally serves this is roughly a minute of sound,
/// which is enough to ride out a range request that stalls for seconds. The
/// decoder itself only buffers `buffer_ms` of decoded samples, so without this
/// a slow request was an audible hole.
const AHEAD_BYTES: usize = 1024 * 1024;

/// Ranges the worker may keep in flight beyond the queue. Two is enough to
/// decouple it from the reader without letting it run through a whole file.
const IN_FLIGHT: usize = 2;

/// One HTTP client for the whole process: unlike `ureq::get`, it reuses the
/// TLS connection between range requests instead of paying for a new handshake
/// (and a fresh DNS lookup) every 16 seconds.
fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::builder()
            .timeout_connect(Duration::from_secs(8))
            .timeout_read(Duration::from_secs(15))
            // Retries are cheap now that the decoder is not the one waiting,
            // but a request that never finishes must not pin a worker forever.
            .timeout(Duration::from_secs(45))
            .build()
    })
}

/// A downloaded range.
struct Chunk {
    start: u64,
    data: Vec<u8>,
    /// Size of the whole stream, when the server told us.
    total: Option<u64>,
}

pub struct HttpFile {
    url: String,
    pos: u64,
    len: Option<u64>,
    chunk: usize,
    /// Downloaded ranges, in order. The front may start before `pos` after a
    /// seek landed inside it.
    queue: VecDeque<(u64, Vec<u8>)>,
    /// Maximum ranges in `queue`. The worker blocks once this much is buffered,
    /// so a long track cannot pull the whole file into memory at full speed.
    ahead: usize,
    /// Offset the worker is expected to deliver next: the end of the queued
    /// data, or the position a seek asked for.
    expected: u64,
    /// Offset at which the stream ended, once the worker has seen the end.
    eof_at: Option<u64>,
    req: Sender<u64>,
    // A `Receiver` is not `Sync`, which symphonia's `MediaSource` requires.
    // Only the reader thread ever locks it, and only to wait for a range.
    rx: Mutex<Receiver<Result<Chunk, String>>>,
    stop: Arc<AtomicBool>,
    /// Offset of the first `moof`/`sidx` box in the file (fragmented MP4), or
    /// `None` for a plain faststart/non-faststart MP4.
    marker: Option<u64>,
    /// False for large fragmented streams: symphonia must not scan them.
    seekable: bool,
}

impl HttpFile {
    /// Opens the stream and pre-fetches the first chunk (also learns the size).
    pub fn open(url: &str, chunk_bytes: usize) -> io::Result<Self> {
        let mut f = Self::start(url, chunk_bytes, 0, true)?;
        let head = f.queue.front().map(|(_, d)| d.as_slice()).unwrap_or(&[]);
        f.marker = fragmented_marker(head);
        // Small fragmented files are cheap to scan (a few dozen fragments), so
        // keep symphonia's random access there; huge ones must not be scanned.
        const FRAGMENTED_SEEK_LIMIT: u64 = 16 * 1024 * 1024;
        f.seekable =
            f.marker.is_none() || f.len.map(|l| l <= FRAGMENTED_SEEK_LIMIT).unwrap_or(false);
        Ok(f)
    }

    /// Opens the stream positioned at `start` (used for the fragment window).
    pub fn open_at(url: &str, chunk_bytes: usize, start: u64) -> io::Result<Self> {
        Self::start(url, chunk_bytes, start, false)
    }

    fn start(url: &str, chunk_bytes: usize, start: u64, seekable: bool) -> io::Result<Self> {
        let chunk = chunk_bytes.max(64 * 1024);
        let ahead = (AHEAD_BYTES / chunk).clamp(2, 16);
        let (req, req_rx) = mpsc::channel();
        let (tx, rx) = mpsc::sync_channel(IN_FLIGHT);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let worker_url = url.to_string();
        thread::Builder::new()
            .name("ytui-range".into())
            .stack_size(256 * 1024)
            .spawn(move || download(worker_url, chunk, start, req_rx, tx, worker_stop))?;

        let mut f = Self {
            url: url.to_string(),
            pos: start,
            len: None,
            chunk,
            queue: VecDeque::new(),
            ahead,
            expected: start,
            eof_at: None,
            req,
            rx: Mutex::new(rx),
            stop,
            marker: None,
            seekable,
        };
        // Wait for the first range like a plain synchronous open would: it also
        // learns the file size, which `SeekFrom::End` and the demuxer need.
        f.wait_chunk()?;
        Ok(f)
    }

    /// Queues one range from the worker. Stale ranges (left over from a
    /// position the reader has since seeked away from) are dropped, and `false`
    /// says nothing useful arrived.
    fn accept(&mut self, msg: Result<Chunk, String>) -> io::Result<bool> {
        let c = msg.map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        if let Some(total) = c.total {
            self.len = Some(total);
        }
        if c.start != self.expected {
            return Ok(false);
        }
        if c.data.is_empty() {
            self.eof_at = Some(c.start);
            return Ok(true);
        }
        self.expected = c.start + c.data.len() as u64;
        self.queue.push_back((c.start, c.data));
        Ok(true)
    }

    /// Moves every finished range into the queue without waiting. This is what
    /// makes the downloader's work visible to the reader: the queue is drained
    /// lazily, so without it `expected` would still point at the first chunk.
    fn fill_ready(&mut self) -> io::Result<()> {
        while self.queue.len() < self.ahead {
            let msg = self
                .rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .try_recv();
            match msg {
                Ok(m) => {
                    self.accept(m)?;
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "range downloader stopped",
                    ))
                }
            }
        }
        Ok(())
    }

    /// Waits for a range the reader can actually use.
    fn wait_chunk(&mut self) -> io::Result<()> {
        loop {
            let msg = self
                .rx
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .recv();
            match msg {
                Ok(m) => {
                    if self.accept(m)? {
                        return Ok(());
                    }
                }
                Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::Other,
                        "range downloader stopped",
                    ))
                }
            }
        }
    }

    /// True once the reader is at or past the end of the stream.
    fn at_eof(&self) -> bool {
        self.eof_at.is_some_and(|end| self.pos >= end)
    }

    /// Lines the download window up with `pos`. A seek inside the downloaded
    /// bytes keeps the queue (and with it the read-ahead); anything else asks
    /// the worker to start over at the new position.
    fn sync(&mut self) {
        if self.at_eof() {
            return;
        }
        let front = self
            .queue
            .front()
            .map(|(start, _)| *start)
            .unwrap_or(self.expected);
        if self.pos >= front && self.pos <= self.expected {
            while let Some((start, data)) = self.queue.front() {
                if self.pos >= start + data.len() as u64 {
                    self.queue.pop_front();
                } else {
                    break;
                }
            }
            return;
        }
        self.restart();
    }

    /// Throws the queue away and asks for a window starting at `pos`.
    fn restart(&mut self) {
        self.queue.clear();
        self.expected = self.pos;
        let _ = self.req.send(self.pos);
    }

    /// Reads an arbitrary range without disturbing the stream position or the
    /// read-ahead window.
    fn read_range(&mut self, start: u64, len: usize) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len);
        let mut at = start;
        while out.len() < len {
            if self.len.is_some_and(|total| at >= total) {
                break;
            }
            let want = (len - out.len()).min(self.chunk);
            let c = fetch_range(&self.url, at, want)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            if self.len.is_none() {
                self.len = c.total;
            }
            if c.data.is_empty() {
                break;
            }
            at += c.data.len() as u64;
            out.extend_from_slice(&c.data);
        }
        if out.len() < len {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short range read",
            ));
        }
        Ok(out)
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

impl Drop for HttpFile {
    fn drop(&mut self) {
        // Wakes a worker waiting for a request. One whose request is in flight
        // notices the closed receiver as soon as it returns.
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// Downloads one range, retrying a few times before giving up.
fn fetch_range(url: &str, start: u64, want: usize) -> Result<Chunk, String> {
    let end = start.saturating_add(want as u64 - 1);
    let mut last = String::from("unknown error");
    for attempt in 0..3u32 {
        let res = agent()
            .get(url)
            .set("Range", &format!("bytes={start}-{end}"))
            .set("User-Agent", UA)
            .call();
        match res {
            Ok(resp) => {
                let status = resp.status();
                if start > 0 && status == 200 {
                    // The server ignored Range and sent the file from byte 0,
                    // which is not what the caller is going to use.
                    last = "server ignored the range".to_string();
                } else {
                    let total = resp
                        .header("Content-Range")
                        .and_then(|cr| cr.rsplit('/').next())
                        .and_then(|s| s.trim().parse().ok())
                        .or_else(|| {
                            // Without Range support only a request from the
                            // start can learn the size from Content-Length.
                            if start == 0 {
                                resp.header("Content-Length")
                                    .and_then(|cl| cl.trim().parse().ok())
                            } else {
                                None
                            }
                        });
                    let mut buf = Vec::with_capacity(want);
                    let mut reader = resp.into_reader();
                    let read = if status == 206 {
                        // The body is exactly the range; reading it to EOF lets
                        // ureq put the connection back in its pool.
                        reader.read_to_end(&mut buf)
                    } else {
                        // A 200 carried the whole file: keep only what we asked
                        // for instead of a whole multi-hour upload.
                        reader.take(want as u64).read_to_end(&mut buf)
                    };
                    match read {
                        Ok(_) if !buf.is_empty() => {
                            return Ok(Chunk {
                                start,
                                data: buf,
                                total,
                            })
                        }
                        Ok(_) => last = "empty range response".to_string(),
                        Err(e) => last = e.to_string(),
                    }
                }
            }
            Err(ureq::Error::Status(416, _)) => {
                // Asked at or past the end of the stream.
                return Ok(Chunk {
                    start,
                    data: Vec::new(),
                    total: None,
                });
            }
            Err(e) => last = e.to_string(),
        }
        thread::sleep(Duration::from_millis(200 * u64::from(attempt + 1)));
    }
    Err(format!("range fetch failed: {last}"))
}

/// Worker thread: fetches ranges ahead of the reader, one at a time.
///
/// It sleeps on the request channel when the queue is full, so a paused track
/// costs nothing. A seek sends a new start offset; a range already in flight is
/// thrown away rather than queued at the wrong place.
fn download(
    url: String,
    chunk: usize,
    mut start: u64,
    req: Receiver<u64>,
    tx: SyncSender<Result<Chunk, String>>,
    stop: Arc<AtomicBool>,
) {
    let mut total: Option<u64> = None;
    loop {
        // A seek replaces the window; only the newest offset matters.
        loop {
            match req.try_recv() {
                Ok(at) => start = at,
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if stop.load(Ordering::Relaxed) {
            return;
        }
        if total.is_some_and(|total| start >= total) {
            if tx
                .send(Ok(Chunk {
                    start,
                    data: Vec::new(),
                    total,
                }))
                .is_err()
            {
                return;
            }
            match req.recv() {
                Ok(at) => {
                    start = at;
                    continue;
                }
                Err(_) => return,
            }
        }

        let fetched = fetch_range(&url, start, chunk);

        // Drop the result if the reader seeked while the request was in flight.
        let mut stale = false;
        loop {
            match req.try_recv() {
                Ok(at) => {
                    start = at;
                    stale = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
        if stale {
            continue;
        }

        match fetched {
            Ok(c) => {
                if total.is_none() {
                    total = c.total;
                }
                let n = c.data.len() as u64;
                let eof = n == 0;
                if tx.send(Ok(c)).is_err() {
                    return;
                }
                if eof {
                    // Nothing more at this position; wait for a seek.
                    match req.recv() {
                        Ok(at) => start = at,
                        Err(_) => return,
                    }
                } else {
                    start += n;
                }
            }
            Err(e) => {
                if tx.send(Err(e)).is_err() {
                    return;
                }
                // A failed range is not retried in a loop: wait for a new
                // request (a seek or the next track) before trying again.
                match req.recv() {
                    Ok(at) => start = at,
                    Err(_) => return,
                }
            }
        }
    }
}

impl Read for HttpFile {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        self.fill_ready()?;
        self.sync();
        loop {
            if let Some((start, data)) = self.queue.front() {
                let end = start + data.len() as u64;
                if self.pos >= end {
                    self.queue.pop_front();
                    continue;
                }
                if self.pos < *start {
                    // The queue moved past the reader; ask for its position.
                    self.restart();
                    continue;
                }
                let off = (self.pos - start) as usize;
                let n = out.len().min(data.len() - off);
                out[..n].copy_from_slice(&data[off..off + n]);
                self.pos += n as u64;
                return Ok(n);
            }
            if self.at_eof() {
                return Ok(0);
            }
            self.wait_chunk()?;
            self.fill_ready()?;
        }
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
        // The queue is only re-synced on the next read, so a demuxer that peeks
        // and seeks back does not throw the read-ahead away.
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
/// it is relative to that position, so the demuxer's absolute seeks stay inside
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

/// Time to byte-offset index built from an MP4 `sidx` box.
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
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;
    use std::time::Instant;

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

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Serves `data` over HTTP Range on localhost and records the requested
    /// `(start, end)` pairs, so a test can see what was prefetched.
    fn serve(data: Vec<u8>) -> (String, Arc<Mutex<Vec<(u64, u64)>>>) {
        serve_with(data, None)
    }

    /// Like `serve`, but the range starting at `gate.0` is held until the
    /// returned flag is set (or three seconds pass), which lets a test prove
    /// the reader does not depend on the range being fetched right now.
    fn serve_with(
        data: Vec<u8>,
        gate: Option<(u64, Arc<AtomicBool>)>,
    ) -> (String, Arc<Mutex<Vec<(u64, u64)>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&hits);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let data = data.clone();
                let seen = Arc::clone(&seen);
                let gate = gate.clone();
                thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() {
                        return;
                    }
                    let mut range = None;
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0 {
                            break;
                        }
                        let header = header.trim_end();
                        if header.is_empty() {
                            break;
                        }
                        if let Some(v) = header.strip_prefix("Range: bytes=") {
                            let (a, b) = v.split_once('-').unwrap();
                            range = Some((a.parse().unwrap(), b.parse().unwrap()));
                        }
                    }
                    let (start, end) = range.unwrap_or((0, data.len() as u64 - 1));
                    let last = data.len() as u64 - 1;
                    if start > last {
                        let _ = stream.write_all(
                            b"HTTP/1.1 416 Range Not Satisfiable\r\nContent-Length: 0\r\n\
                              Connection: close\r\n\r\n",
                        );
                        return;
                    }
                    let end = end.min(last);
                    seen.lock().unwrap().push((start, end));
                    if let Some((at, open)) = &gate {
                        if start == *at {
                            let deadline = Instant::now() + Duration::from_secs(3);
                            while !open.load(Ordering::Relaxed) && Instant::now() < deadline {
                                thread::sleep(Duration::from_millis(5));
                            }
                        }
                    }
                    let slice = &data[start as usize..=end as usize];
                    let head = format!(
                        "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                         Content-Range: bytes {}-{}/{}\r\nConnection: close\r\n\r\n",
                        slice.len(),
                        start,
                        end,
                        data.len()
                    );
                    let _ = stream.write_all(head.as_bytes());
                    let _ = stream.write_all(slice);
                });
            }
        });
        (format!("http://{addr}/audio"), hits)
    }

    fn wait_for(seen: &Arc<Mutex<Vec<(u64, u64)>>>, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while seen.lock().unwrap().len() < n && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn downloads_ahead_and_serves_the_stream_in_order() {
        let data = body(300 * 1024);
        let (url, hits) = serve(data.clone());
        let mut f = HttpFile::open(&url, 64 * 1024).unwrap();
        assert_eq!(f.byte_len(), Some(data.len() as u64));
        // The worker keeps pulling ranges while the decoder is still busy, so
        // a stalled request does not drain the audio ring.
        wait_for(&hits, 2);
        let requested = hits.lock().unwrap().clone();
        assert!(
            requested.len() >= 2,
            "expected read-ahead, got {requested:?}"
        );
        assert_eq!(requested[0].0, 0);

        let mut got = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = f.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, data);
    }

    #[test]
    fn seeks_forward_backward_and_beyond_the_downloaded_window() {
        let data = body(300 * 1024);
        let (url, _) = serve(data.clone());
        let mut f = HttpFile::open(&url, 64 * 1024).unwrap();
        let mut buf = [0u8; 1000];

        // Just ahead of the start, still inside the read-ahead.
        f.seek(SeekFrom::Start(70_000)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[70_000..71_000]);

        // Far ahead, past anything downloaded, starts a new window.
        f.seek(SeekFrom::Start(250_000)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[250_000..251_000]);

        // Backwards, before the window.
        f.seek(SeekFrom::Start(5)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[5..1005]);

        // Running to the end marks the stream as finished, but seeking back
        // afterwards has to work again (a demuxer does this on loop/replay).
        let mut rest = Vec::new();
        f.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[1005..]);
        f.seek(SeekFrom::Start(6)).unwrap();
        f.read_exact(&mut buf).unwrap();
        assert_eq!(&buf[..], &data[6..1006]);
    }

    /// A range request that stalls must not starve the decoder: everything
    /// already downloaded keeps flowing while the request is in flight.
    #[test]
    fn a_stalled_range_does_not_stop_the_reader() {
        let chunk = 64 * 1024;
        let data = body(8 * chunk);
        let open = Arc::new(AtomicBool::new(false));
        let (url, _) = serve_with(
            data.clone(),
            Some((3 * chunk as u64, Arc::clone(&open))),
        );
        let mut f = HttpFile::open(&url, chunk).unwrap();

        // Ends exactly at a range boundary, with the next range still stalled.
        let start = Instant::now();
        let mut buf = vec![0u8; 3 * chunk];
        f.read_exact(&mut buf).unwrap();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the reader waited for the stalled range"
        );
        assert_eq!(buf, data[..3 * chunk]);

        open.store(true, Ordering::Relaxed);
        let mut rest = Vec::new();
        f.read_to_end(&mut rest).unwrap();
        assert_eq!(rest, &data[3 * chunk..]);
    }

    #[test]
    fn read_range_does_not_move_the_stream() {
        let data = body(200 * 1024);
        let (url, _) = serve(data.clone());
        let mut f = HttpFile::open(&url, 64 * 1024).unwrap();
        assert_eq!(f.read_range(1000, 100).unwrap(), &data[1000..1100]);
        assert_eq!(f.pos, 0);
        // Crosses a range boundary, so it takes more than one request.
        assert_eq!(f.read_range(63_000, 3000).unwrap(), &data[63_000..66_000]);
    }
}
