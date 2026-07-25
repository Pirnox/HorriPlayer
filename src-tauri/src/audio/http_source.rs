//! A seekable `MediaSource` over HTTP, so the decoder can read server streams
//! directly instead of going through the WebView.
//!
//! Symphonia needs `Read + Seek + Send + Sync`, which a live response body is
//! not. Instead this keeps one window of the file in memory and refills it with
//! Range requests, so seeking costs one request rather than a re-download.

use std::io::{self, Read, Seek, SeekFrom};
use std::time::Duration;

use symphonia::core::io::MediaSource;

/// Window size per Range request. Large enough that sequential playback issues
/// few requests, small enough that a seek does not pull megabytes it won't use.
const CHUNK: u64 = 1 << 20; // 1 MiB

pub struct HttpSource {
    client: reqwest::blocking::Client,
    url: String,
    pos: u64,
    len: Option<u64>,
    buf: Vec<u8>,
    buf_start: u64,
    /// Set when the server ignored our Range header and sent the whole body.
    whole_body: bool,
}

impl HttpSource {
    pub fn new(url: String) -> io::Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(io_err)?;
        let mut src = Self {
            client,
            url,
            pos: 0,
            len: None,
            buf: Vec::new(),
            buf_start: 0,
            whole_body: false,
        };
        // Prime the first window; this also discovers the length.
        src.fill_at(0)?;
        Ok(src)
    }

    fn fill_at(&mut self, start: u64) -> io::Result<()> {
        if self.whole_body && !self.buf.is_empty() {
            return Ok(()); // already hold everything
        }
        let end = start.saturating_add(CHUNK).saturating_sub(1);
        let resp = self
            .client
            .get(&self.url)
            .header("Range", format!("bytes={start}-{end}"))
            .send()
            .map_err(io_err)?;

        let status = resp.status();
        if !status.is_success() {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("HTTP {status} while fetching audio"),
            ));
        }

        // 206 means the range was honoured; 200 means we got the entire file.
        let partial = status.as_u16() == 206;
        if let Some(total) = resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.rsplit('/').next().map(|t| t.trim().to_string()))
            .and_then(|t| t.parse::<u64>().ok())
        {
            self.len = Some(total);
        } else if !partial {
            self.len = resp.content_length();
        }

        let body = resp.bytes().map_err(io_err)?;
        self.buf = body.to_vec();
        if partial {
            self.buf_start = start;
        } else {
            self.buf_start = 0;
            self.whole_body = true;
            if self.len.is_none() {
                self.len = Some(self.buf.len() as u64);
            }
        }
        Ok(())
    }

    fn buf_contains(&self, pos: u64) -> bool {
        pos >= self.buf_start && pos < self.buf_start + self.buf.len() as u64
    }
}

fn io_err<E: std::fmt::Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

impl Read for HttpSource {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        if out.is_empty() {
            return Ok(0);
        }
        if let Some(len) = self.len {
            if self.pos >= len {
                return Ok(0); // clean EOF
            }
        }
        if !self.buf_contains(self.pos) {
            self.fill_at(self.pos)?;
            if !self.buf_contains(self.pos) {
                return Ok(0); // past the end
            }
        }
        let offset = (self.pos - self.buf_start) as usize;
        let available = self.buf.len() - offset;
        let n = available.min(out.len());
        out[..n].copy_from_slice(&self.buf[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for HttpSource {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let target = match from {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => match self.len {
                Some(len) => len as i64 + d,
                None => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "stream length unknown",
                    ))
                }
            },
        };
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek before start of stream",
            ));
        }
        self.pos = target as u64;
        Ok(self.pos)
    }
}

impl MediaSource for HttpSource {
    fn is_seekable(&self) -> bool {
        self.len.is_some()
    }

    fn byte_len(&self) -> Option<u64> {
        self.len
    }
}
