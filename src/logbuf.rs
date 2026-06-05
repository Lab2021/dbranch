//! In-memory ring buffer of dBranch's own tracing output, exposed to the
//! Web UI via `GET /api/logs`.
//!
//! Wired into `tracing_subscriber` as an additional fmt layer (in
//! `main::main`) — `with_writer(buffer)` makes every formatted log record
//! also land here. The stderr fmt layer stays so terminal output is
//! unchanged.
//!
//! Bounded: oldest lines drop off once the capacity is reached, so memory
//! usage is constant regardless of how long the process runs.

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, OnceLock};

use tracing_subscriber::fmt::MakeWriter;

/// Default ring capacity. ~2000 short lines ≈ ~250 KB of memory.
pub const DEFAULT_CAPACITY: usize = 2000;

#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<VecDeque<String>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    /// Returns the last `tail` lines (or all of them if `tail` exceeds the
    /// buffer's size). `tail = 0` returns everything currently buffered.
    pub fn snapshot(&self, tail: usize) -> Vec<String> {
        let q = self.inner.lock().unwrap();
        let take = if tail == 0 { q.len() } else { tail.min(q.len()) };
        let start = q.len() - take;
        q.iter().skip(start).cloned().collect()
    }

    fn push_line(&self, line: String) {
        if line.is_empty() {
            return;
        }
        let mut q = self.inner.lock().unwrap();
        while q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back(line);
    }
}

static GLOBAL: OnceLock<LogBuffer> = OnceLock::new();

/// Installs the global buffer on first call; idempotent (further calls
/// return the same buffer regardless of `capacity`). Returns a clone safe to
/// hand to `tracing_subscriber::fmt::layer().with_writer(...)`.
pub fn install(capacity: usize) -> LogBuffer {
    GLOBAL.get_or_init(|| LogBuffer::new(capacity)).clone()
}

/// Returns the global buffer if [`install`] has been called.
pub fn global() -> Option<&'static LogBuffer> {
    GLOBAL.get()
}

/// `Write` adapter passed to `tracing_subscriber`'s fmt layer.
///
/// Each tracing event allocates a fresh `BufferWriter` via [`MakeWriter`],
/// the layer writes the formatted record into it, and on drop we split the
/// accumulated bytes by newline and push each line into the ring.
pub struct BufferWriter {
    buf: Vec<u8>,
    sink: LogBuffer,
}

impl Write for BufferWriter {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for BufferWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        let text = String::from_utf8_lossy(&self.buf).into_owned();
        for line in text.lines() {
            self.sink.push_line(line.to_string());
        }
    }
}

impl<'a> MakeWriter<'a> for LogBuffer {
    type Writer = BufferWriter;
    fn make_writer(&'a self) -> BufferWriter {
        BufferWriter {
            buf: Vec::new(),
            sink: self.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_returns_all_lines_when_tail_zero() {
        let buf = LogBuffer::new(10);
        for i in 0..3 {
            buf.push_line(format!("line {}", i));
        }
        let lines = buf.snapshot(0);
        assert_eq!(lines, vec!["line 0", "line 1", "line 2"]);
    }

    #[test]
    fn snapshot_caps_at_tail() {
        let buf = LogBuffer::new(10);
        for i in 0..5 {
            buf.push_line(format!("L{}", i));
        }
        let lines = buf.snapshot(2);
        assert_eq!(lines, vec!["L3", "L4"]);
    }

    #[test]
    fn ring_drops_oldest_at_capacity() {
        let buf = LogBuffer::new(3);
        for i in 0..6 {
            buf.push_line(format!("L{}", i));
        }
        assert_eq!(buf.snapshot(0), vec!["L3", "L4", "L5"]);
    }

    #[test]
    fn buffer_writer_splits_by_newline() {
        let buf = LogBuffer::new(10);
        {
            let mut w = BufferWriter {
                buf: Vec::new(),
                sink: buf.clone(),
            };
            writeln!(w, "first").unwrap();
            writeln!(w, "second").unwrap();
        } // drop pushes
        assert_eq!(buf.snapshot(0), vec!["first", "second"]);
    }
}
