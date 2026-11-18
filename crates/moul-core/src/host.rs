//! The outside world, as the engine sees it: a clock and randomness.
//!
//! Everything the engine cannot compute comes through [`Host`], so a test
//! can replay a run exactly with [`SeededHost`]. Storage comes through the
//! [`Store`](moul_store::Store) and uploaded files through
//! [`Blobs`](crate::Blobs).

/// What the engine needs from where it runs.
pub trait Host {
    /// Milliseconds since the Unix epoch, UTC.
    fn now_ms(&mut self) -> i64;

    /// Fills `buf` with random bytes good enough for keys and tokens.
    fn fill_random(&mut self, buf: &mut [u8]);

    /// A clock SQLite's progress handler can read while a statement runs,
    /// in milliseconds on any fixed origin. It must be a plain function
    /// because the handler cannot borrow the host. `None` turns the time
    /// budget off and leaves only the step and row budgets, which is what a
    /// replayable test wants.
    fn statement_clock(&self) -> Option<fn() -> i64> {
        None
    }
}

/// A host for native programs: the system clock and the operating
/// system's randomness.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemHost;

#[cfg(not(target_arch = "wasm32"))]
impl Host for SystemHost {
    fn now_ms(&mut self) -> i64 {
        system_ms()
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        getrandom::fill(buf).expect("the operating system supplies randomness");
    }

    fn statement_clock(&self) -> Option<fn() -> i64> {
        Some(system_ms)
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn system_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A host whose clock and randomness are fixed by a seed, for tests that
/// must replay exactly. The clock starts at `start_ms` and moves only when
/// told to.
#[derive(Debug, Clone)]
pub struct SeededHost {
    now_ms: i64,
    state: [u64; 4],
}

impl SeededHost {
    /// A host at `start_ms` whose randomness comes from `seed`.
    pub fn new(seed: u64, start_ms: i64) -> SeededHost {
        // SplitMix64 expands the seed into xoshiro256** state.
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        SeededHost {
            now_ms: start_ms,
            state: [next(), next(), next(), next()],
        }
    }

    /// Moves the clock forward by `ms`.
    pub fn advance(&mut self, ms: i64) {
        self.now_ms += ms;
    }

    /// Sets the clock.
    pub fn set_now_ms(&mut self, ms: i64) {
        self.now_ms = ms;
    }

    fn next_u64(&mut self) -> u64 {
        let s = &mut self.state;
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }
}

impl Host for SeededHost {
    fn now_ms(&mut self) -> i64 {
        self.now_ms
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        for chunk in buf.chunks_mut(8) {
            let bytes = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
}

impl<H: Host + ?Sized> Host for Box<H> {
    fn now_ms(&mut self) -> i64 {
        (**self).now_ms()
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        (**self).fill_random(buf)
    }

    fn statement_clock(&self) -> Option<fn() -> i64> {
        (**self).statement_clock()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_hosts_repeat() {
        let mut a = SeededHost::new(7, 0);
        let mut b = SeededHost::new(7, 0);
        let (mut x, mut y) = ([0u8; 13], [0u8; 13]);
        a.fill_random(&mut x);
        b.fill_random(&mut y);
        assert_eq!(x, y);
        assert_ne!(x, [0; 13]);
    }
}
