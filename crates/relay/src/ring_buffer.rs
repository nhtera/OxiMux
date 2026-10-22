// Fixed-capacity byte ring buffer for per-PTY scrollback replay. When
// the buffer fills, the oldest bytes are silently overwritten — the
// daemon never blocks the PTY reader on slow consumers.
//
// The storage grows with the output, up to the capacity: an idle shell holds
// a prompt's worth, not a megabyte (measured: 200 idle PTYs held ~1 MiB of
// resident memory each with the ring allocated up front).

pub struct RingBuffer {
    // Grows to `cap`, then stays; until then it holds exactly the bytes
    // written, oldest first, and `head == len`.
    buf: Vec<u8>,
    cap: usize,
    // Position to write the next byte. Always in `[0, capacity)`.
    head: usize,
    // Total bytes currently stored; `min(written, capacity)`.
    len: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "RingBuffer capacity must be > 0");
        Self {
            buf: Vec::new(),
            cap: capacity,
            head: 0,
            len: 0,
        }
    }

    pub fn capacity(&self) -> usize {
        self.cap
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn push(&mut self, bytes: &[u8]) {
        let cap = self.cap;
        // Inputs larger than capacity overwrite themselves; only the
        // last `cap` bytes matter.
        let mut bytes = if bytes.len() > cap {
            &bytes[bytes.len() - cap..]
        } else {
            bytes
        };
        // Still growing: append (doubling the storage, capped at `cap`), and
        // wrap only what does not fit.
        if self.buf.len() < cap {
            let take = bytes.len().min(cap - self.buf.len());
            if self.buf.capacity() < self.buf.len() + take {
                let want = (self.buf.capacity() * 2).max(self.buf.len() + take).max(4096).min(cap);
                self.buf.reserve_exact(want - self.buf.len());
            }
            self.buf.extend_from_slice(&bytes[..take]);
            self.len = self.buf.len();
            self.head = self.buf.len() % cap;
            bytes = &bytes[take..];
        }
        let n = bytes.len();
        if n == 0 {
            return;
        }
        let space_after_head = cap - self.head;
        if n <= space_after_head {
            self.buf[self.head..self.head + n].copy_from_slice(bytes);
        } else {
            let first = space_after_head;
            let second = n - first;
            self.buf[self.head..cap].copy_from_slice(&bytes[..first]);
            self.buf[..second].copy_from_slice(&bytes[first..]);
        }
        self.head = (self.head + n) % cap;
        self.len = (self.len + n).min(cap);
    }

    // Return the buffered bytes in chronological (oldest-first) order.
    pub fn snapshot(&self) -> Vec<u8> {
        if self.len == 0 {
            return Vec::new();
        }
        let cap = self.cap;
        let start = (self.head + cap - self.len) % cap;
        let mut out = Vec::with_capacity(self.len);
        if start + self.len <= cap {
            out.extend_from_slice(&self.buf[start..start + self.len]);
        } else {
            let first = cap - start;
            out.extend_from_slice(&self.buf[start..cap]);
            out.extend_from_slice(&self.buf[..self.len - first]);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_empty() {
        let rb = RingBuffer::new(16);
        assert!(rb.is_empty());
        assert_eq!(rb.snapshot(), Vec::<u8>::new());
    }

    #[test]
    fn push_within_capacity() {
        let mut rb = RingBuffer::new(16);
        rb.push(b"hello");
        assert_eq!(rb.len(), 5);
        assert_eq!(rb.snapshot(), b"hello");
    }

    #[test]
    fn push_wraps_around() {
        let mut rb = RingBuffer::new(8);
        rb.push(b"abcdef"); // 6 bytes
        rb.push(b"ghij"); // 4 bytes; total written = 10, cap = 8
        // Snapshot should be the last 8 bytes: "cdefghij".
        assert_eq!(rb.snapshot(), b"cdefghij");
    }

    #[test]
    fn push_larger_than_capacity_keeps_tail() {
        let mut rb = RingBuffer::new(4);
        rb.push(b"abcdefghij"); // 10 bytes into cap=4
        assert_eq!(rb.snapshot(), b"ghij");
        assert_eq!(rb.len(), 4);
    }

    #[test]
    fn repeated_pushes_match_naive_tail() {
        // Compare against a naive implementation: keep a Vec, push,
        // truncate from the front. Random byte patterns.
        let cap = 32;
        let mut rb = RingBuffer::new(cap);
        let mut naive: Vec<u8> = Vec::new();
        let mut state: u32 = 0x12345678;
        for _ in 0..200 {
            state = state.wrapping_mul(1103515245).wrapping_add(12345);
            let n = (state as usize % 20) + 1;
            let bytes: Vec<u8> = (0..n)
                .map(|i| ((state >> (i % 4 * 8)) & 0xFF) as u8)
                .collect();
            rb.push(&bytes);
            naive.extend_from_slice(&bytes);
            if naive.len() > cap {
                let drop = naive.len() - cap;
                naive.drain(..drop);
            }
            assert_eq!(rb.snapshot(), naive, "mismatch after push of {n} bytes");
        }
    }

    #[test]
    fn storage_grows_with_output_and_capacity_is_unchanged() {
        let mut rb = RingBuffer::new(1 << 20);
        assert_eq!(rb.capacity(), 1 << 20);
        rb.push(b"$ ");
        assert!(rb.buf.capacity() <= 4096, "an idle prompt holds {} bytes", rb.buf.capacity());
        assert_eq!(rb.snapshot(), b"$ ");
        assert_eq!(rb.capacity(), 1 << 20);
    }

    #[test]
    fn growth_then_wrap_matches_naive_tail_at_every_step() {
        let cap = 10_000;
        let mut rb = RingBuffer::new(cap);
        let mut naive: Vec<u8> = Vec::new();
        for i in 0..400u32 {
            let chunk: Vec<u8> = (0..(i * 37 % 300)).map(|j| (i + j) as u8).collect();
            rb.push(&chunk);
            naive.extend_from_slice(&chunk);
            let tail = &naive[naive.len().saturating_sub(cap)..];
            assert_eq!(rb.snapshot(), tail, "after push {i}");
            assert!(rb.buf.len() <= cap && rb.buf.capacity() <= cap);
        }
    }
}
