//! The engine without a worker, tabs or browser storage.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::Function;
use moul_core::{Config, Engine as Core, Host, SeededHost, StoreBlobs};
use wasm_bindgen::prelude::*;

use crate::host::{self, Engine, Push};
use crate::js;
use crate::webhost::WebHost;

/// The clock and randomness of a headless engine: the real ones, or a
/// clock that moves only when told and randomness from a seed, so a test
/// replays exactly.
struct HeadlessHost {
    clock: Option<Rc<Cell<i64>>>,
    seeded: Option<SeededHost>,
}

impl Host for HeadlessHost {
    fn now_ms(&mut self) -> i64 {
        match &self.clock {
            Some(c) => c.get(),
            None => WebHost.now_ms(),
        }
    }

    fn fill_random(&mut self, buf: &mut [u8]) {
        match &mut self.seeded {
            Some(s) => s.fill_random(buf),
            None => WebHost.fill_random(buf),
        }
    }

    fn statement_clock(&self) -> Option<fn() -> i64> {
        // A fixed clock turns the time budget off, as a replayable test
        // wants; the step and row budgets still apply.
        match self.clock {
            Some(_) => None,
            None => WebHost.statement_clock(),
        }
    }
}

/// The engine in memory, answering in the calling thread: no worker, no
/// tabs, no OPFS. For tests in Node and for tools that want an engine of
/// their own. It takes the same requests as [`MoulWorker`](crate::MoulWorker)
/// (`api`, `subscribe`, `export`, `import`, `status`, …) and pushes the
/// same notices and events to the function set with `onPush`.
///
/// ```js
/// import init, { MoulEngine } from "./moul_web.js";
/// await init({ module_or_path: wasmBytes });
/// const engine = new MoulEngine({ now: Date.parse("2026-11-18"), seed: 7 });
/// engine.request({ op: "api", method: "POST", path: "/api/moul", owner: true,
///                  json: { name: "notes", fields: [{ name: "text", type: "text" }] } });
/// ```
///
/// Options: `now` fixes the clock at that many milliseconds since the Unix
/// epoch (move it with `advance`), and `seed` makes ids, tokens and salts
/// repeat from run to run.
#[wasm_bindgen]
pub struct MoulEngine {
    engine: Engine,
    listener: Rc<RefCell<Option<Function>>>,
    clock: Option<Rc<Cell<i64>>>,
}

impl std::fmt::Debug for MoulEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MoulEngine").finish_non_exhaustive()
    }
}

#[wasm_bindgen]
impl MoulEngine {
    /// Opens an empty engine in memory.
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> Result<MoulEngine, JsValue> {
        let options = if options.is_object() {
            options
        } else {
            js::object(&[])
        };
        let clock = js::get(&options, "now")
            .as_f64()
            .map(|ms| Rc::new(Cell::new(ms as i64)));
        let seeded = js::get(&options, "seed")
            .as_f64()
            .map(|seed| SeededHost::new(seed as u64, 0));
        let listener: Rc<RefCell<Option<Function>>> = Rc::new(RefCell::new(None));
        let to_listener = listener.clone();
        let push: Push = Rc::new(move |_to, body| {
            let f = to_listener.borrow().clone();
            if let Some(f) = f {
                let _ = f.call1(&JsValue::NULL, &body);
            }
        });
        let opened = host::open_memory(
            "headless",
            Some("a headless engine keeps its data in memory".into()),
        )?;
        let host = HeadlessHost {
            clock: clock.clone(),
            seeded,
        };
        let engine = Engine::with_core(opened, None, push, move |slot| {
            Core::open(slot.take(), host, StoreBlobs, Config::default())
        })?;
        Ok(MoulEngine {
            engine,
            listener,
            clock,
        })
    }

    /// Answers one request. Sync requests answer with a promise.
    pub fn request(&self, req: JsValue) -> Result<JsValue, JsValue> {
        self.engine.handle(req, "local")
    }

    /// Sets the function that receives change notices and subscription
    /// events. `null` removes it.
    #[wasm_bindgen(js_name = onPush)]
    pub fn on_push(&self, listener: Option<Function>) {
        *self.listener.borrow_mut() = listener;
    }

    /// Moves a fixed clock forward by `ms`. Without `now` the engine reads
    /// the real clock and this does nothing.
    pub fn advance(&self, ms: f64) {
        if let Some(c) = &self.clock {
            c.set(c.get() + ms as i64);
        }
    }
}
