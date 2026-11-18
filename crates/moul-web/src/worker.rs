//! The entry point a dedicated worker starts.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::DedicatedWorkerGlobalScope;

use crate::host::{self, Engine, WeakEngine};
use crate::js;
use crate::leader::{Leadership, lock_name};
use crate::persist::{self, PERSIST_MESSAGE, Persistence};
use crate::tabs::Tabs;

#[derive(Default)]
struct State {
    leadership: Option<Leadership>,
    closed: bool,
    error: Option<String>,
}

/// Runs in each tab's dedicated worker. Every tab can send requests; the one
/// that wins the leader lock opens the database and answers them all.
///
/// ```js
/// // worker.js
/// import init, { MoulWorker } from "./moul_web.js";
/// await init();
/// const moul = new MoulWorker("notes");
/// await moul.request({ op: "exec", sql: "CREATE TABLE IF NOT EXISTS notes (body TEXT)" });
/// ```
///
/// After the first write, a leader whose storage is not yet persistent posts
/// `{ type: "moul:persist" }` to its page, which should call
/// `requestPersistence()`.
#[wasm_bindgen]
pub struct MoulWorker {
    db: String,
    tabs: Rc<Tabs>,
    state: Rc<RefCell<State>>,
}

#[wasm_bindgen]
impl MoulWorker {
    /// Joins the tabs of database `db` and starts competing for the leader lock.
    #[wasm_bindgen(constructor)]
    pub fn new(db: &str) -> Result<MoulWorker, JsValue> {
        if db.is_empty() || db.contains(['/', '\\']) {
            return Err(js::error(
                "a database name must be non-empty and have no slashes",
            ));
        }
        let tabs = Rc::new(Tabs::join(&lock_name(db))?);
        let state = Rc::new(RefCell::new(State::default()));
        spawn_local(lead(db.to_owned(), tabs.clone(), state.clone()));
        Ok(MoulWorker {
            db: db.to_owned(),
            tabs,
            state,
        })
    }

    /// Sends a request to the leading tab, which may be this one.
    pub async fn request(&self, req: JsValue) -> Result<JsValue, JsValue> {
        self.tabs.request(req).await
    }

    /// Whether this tab holds the database.
    #[wasm_bindgen(getter, js_name = isLeader)]
    pub fn is_leader(&self) -> bool {
        self.tabs.is_leader()
    }

    /// The database name.
    #[wasm_bindgen(getter)]
    pub fn db(&self) -> String {
        self.db.clone()
    }

    /// The last error from opening the database, if the last attempt failed.
    #[wasm_bindgen(getter, js_name = lastError)]
    pub fn last_error(&self) -> Option<String> {
        self.state.borrow().error.clone()
    }

    /// Stops leading, closes the files and lets the next tab take over.
    pub fn close(&self) {
        let mut state = self.state.borrow_mut();
        state.closed = true;
        // Close the files before releasing the lock, so the next leader can
        // open them at once.
        drop(self.tabs.resign());
        state.leadership = None;
    }
}

async fn lead(db: String, tabs: Rc<Tabs>, state: Rc<RefCell<State>>) {
    loop {
        let leadership = match Leadership::acquire(&lock_name(&db)).await {
            Ok(l) => l,
            Err(e) => {
                state.borrow_mut().error = Some(js::message(&e));
                return;
            }
        };
        if state.borrow().closed {
            return;
        }
        match host::open(&db).await {
            Ok(opened) => {
                let durable = opened.durable;
                let first_write: Option<Box<dyn FnOnce()>> = durable
                    .then(|| Box::new(|| spawn_local(ask_page_to_persist())) as Box<dyn FnOnce()>);
                let engine = Engine::new(opened, first_write);
                spawn_local(sync_in_background(engine.downgrade()));
                tabs.lead(Box::new(move |req| engine.handle(req)));
                let mut s = state.borrow_mut();
                s.error = None;
                s.leadership = Some(leadership);
                return;
            }
            Err(e) => {
                // Usually the previous leader has not let go of the files yet.
                state.borrow_mut().error = Some(js::message(&e));
                drop(leadership);
                js::sleep_ms(1_000).await;
            }
        }
    }
}

/// Pushes and pulls about once a second while the engine is open. Holds the
/// engine only during a round, so closing it is not held up between rounds.
async fn sync_in_background(engine: WeakEngine) {
    loop {
        js::sleep_ms(1_000).await;
        let Some(engine) = engine.upgrade() else {
            return;
        };
        engine.tick().await;
    }
}

async fn ask_page_to_persist() {
    if persist::persisted().await == Persistence::Persistent {
        return;
    }
    if let Ok(scope) = js_sys::global().dyn_into::<DedicatedWorkerGlobalScope>() {
        let _ = scope.post_message(&js::object(&[("type", PERSIST_MESSAGE.into())]));
    }
}
