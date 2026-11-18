//! The engine's clock and randomness in a browser, a worker or Node.

use js_sys::Uint8Array;
use moul_core::Host;

use crate::js;

/// `Date.now()` and `crypto.getRandomValues`, which pages, workers and
/// Node all have.
#[derive(Clone, Copy, Debug, Default)]
pub struct WebHost;

fn date_now() -> i64 {
    js_sys::Date::now() as i64
}

impl Host for WebHost {
    fn now_ms(&mut self) -> i64 {
        date_now()
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        let crypto = js::get(&js::global(), "crypto");
        // getRandomValues fills at most 65,536 bytes a call.
        for chunk in buf.chunks_mut(65_536) {
            let array = Uint8Array::new_with_length(chunk.len() as u32);
            js::call(&crypto, "getRandomValues", &[array.clone().into()])
                .expect("crypto.getRandomValues is available");
            array.copy_to(chunk);
        }
    }

    fn statement_clock(&self) -> Option<fn() -> i64> {
        Some(date_now)
    }
}
