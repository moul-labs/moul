//! The entry point a dedicated worker starts.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;
use web_sys::DedicatedWorkerGlobalScope;

use crate::host::{self, Engine, Push, WeakEngine};
use crate::js;
use crate::leader::{Leadership, lock_name};
use crate::persist::{self, PERSIST_MESSAGE, Persistence};
use crate::tabs::{self, Tabs};

#[derive(Default)]
struct State {
    leadership: Option<Leadership>,
    /// The engine this tab leads with, without keeping it open.
    engine: Option<WeakEngine>,
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
///
/// Notices, subscription events and leader changes reach the function set
/// with `onPush`; see the `host` and `tabs` modules for their shapes.
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
        MoulWorker::with_retry(db, Retry::default())
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

    /// This tab's id, as subscriptions and pushes name it.
    #[wasm_bindgen(getter, js_name = tabId)]
    pub fn tab_id(&self) -> String {
        self.tabs.tab_id().to_owned()
    }

    /// Sets the function that receives what the leader pushes to this tab:
    /// change notices, subscription events, `{ type: "leader", tab }` when
    /// another tab takes over the database, and `{ type: "leader", tab,
    /// opened: true }` when a leader that answered with an error because the
    /// database would not open has opened it. `null` removes it.
    #[wasm_bindgen(js_name = onPush)]
    pub fn on_push(&self, listener: Option<js_sys::Function>) {
        self.tabs.on_push(listener);
    }

    /// The last error from opening the database, if the last attempt failed.
    #[wasm_bindgen(getter, js_name = lastError)]
    pub fn last_error(&self) -> Option<String> {
        self.state.borrow().error.clone()
    }

    /// Stops leading, closes the files and lets the next tab take over.
    ///
    /// Background sync stops. The leader lock is released once nothing
    /// holds the engine any more, so the next leader can open the files at
    /// once: a sync round that is waiting on the network finishes first,
    /// for at most 30 seconds.
    pub fn close(&self) {
        let (engine, leadership) = {
            let mut state = self.state.borrow_mut();
            state.closed = true;
            (state.engine.take(), state.leadership.take())
        };
        if let Some(e) = engine.as_ref().and_then(WeakEngine::upgrade) {
            e.shut_down();
        }
        // The handler holds the engine: dropping it closes the files unless
        // a sync round still holds them.
        drop(self.tabs.resign());
        let Some(leadership) = leadership else {
            return;
        };
        match engine {
            Some(engine) if engine.is_open() => spawn_local(async move {
                let mut waited = 0;
                while engine.is_open() && waited < CLOSE_WAIT_MS {
                    js::sleep_ms(50).await;
                    waited += 50;
                }
                drop(leadership);
            }),
            _ => drop(leadership),
        }
    }
}

impl MoulWorker {
    /// [`MoulWorker::new`], trying to open the database as `retry` says.
    /// For tests.
    #[doc(hidden)]
    pub fn with_retry(db: &str, retry: Retry) -> Result<MoulWorker, JsValue> {
        if db.is_empty() || db.contains(['/', '\\']) || db.ends_with(crate::conflicts::SUFFIX) {
            return Err(js::error(
                "a database name must be non-empty, have no slashes and not end in \"-conflicts\"",
            ));
        }
        let tabs = Rc::new(Tabs::join(&lock_name(db))?);
        let state = Rc::new(RefCell::new(State::default()));
        spawn_local(lead(
            db.to_owned(),
            Rc::downgrade(&tabs),
            Rc::downgrade(&state),
            retry,
        ));
        Ok(MoulWorker {
            db: db.to_owned(),
            tabs,
            state,
        })
    }
}

/// How long [`MoulWorker::close`] waits for a running sync round to let go
/// of the engine before it releases the leader lock anyway.
const CLOSE_WAIT_MS: u32 = 30_000;

/// How the leader tries to open the database.
///
/// It tries `attempts` times, `delay_ms` apart, letting go of the leader
/// lock in between. Then it keeps the lock and answers every request with
/// the error, so pages do not wait forever, and goes on trying with longer
/// and longer waits, up to `max_delay_ms`. Once the database opens, it
/// answers with it, without a reload.
#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct Retry {
    /// Attempts before requests are answered with the error.
    pub attempts: u32,
    /// The wait between those attempts.
    pub delay_ms: u32,
    /// The longest wait between the attempts after them.
    pub max_delay_ms: u32,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 10,
            delay_ms: 1_000,
            max_delay_ms: 5_000,
        }
    }
}

/// After this many refusals of OPFS in a row for no stated reason
/// (WebKit's private windows refuse it with `UnknownError`, from
/// `getDirectory()` or the sync access handles on), the next one runs the
/// engine in memory and says so, unless OPFS already holds a database for
/// it: see [`host::stored`].
const UNKNOWN_REFUSALS: u32 = 2;

/// Opens the database while the worker lives, and leads with it. Holds the
/// worker's tabs and state only between waits: a worker dropped without
/// [`MoulWorker::close`] stops this, and lets go of the lock.
async fn lead(db: String, tabs_ref: Weak<Tabs>, state_ref: Weak<RefCell<State>>, retry: Retry) {
    let mut failures: u32 = 0;
    let mut refusals: u32 = 0;
    // OPFS holds this database, but the browser refused to open it.
    let mut refused_existing = false;
    // Past the attempts, the lock is kept from one attempt to the next.
    let mut kept: Option<Leadership> = None;
    // What requests are answered with meanwhile, once they are.
    let mut answer: Option<Rc<RefCell<String>>> = None;
    loop {
        let leadership = match kept.take() {
            Some(l) => l,
            None => match Leadership::acquire(&lock_name(&db)).await {
                Ok(l) => l,
                Err(e) => {
                    if let Some(state) = state_ref.upgrade() {
                        state.borrow_mut().error = Some(js::message(&e));
                    }
                    return;
                }
            },
        };
        if state_ref.upgrade().is_none_or(|s| s.borrow().closed) {
            return;
        }
        let mut opened = host::open(&db).await;
        if let Err(e) = &opened {
            if js::message(e).contains("UnknownError") {
                if refusals >= UNKNOWN_REFUSALS {
                    match host::stored(&db).await {
                        host::Stored::Data => refused_existing = true,
                        host::Stored::Nothing => {
                            opened = host::open_memory(
                                &db,
                                Some(format!(
                                    "the browser refused its storage, as private windows do, so \
                                     the data lives in memory until the tab closes ({})",
                                    js::message(e)
                                )),
                            );
                        }
                        host::Stored::Refused(why) => {
                            opened = host::open_memory(
                                &db,
                                Some(format!(
                                    "the browser refused its file system altogether, as private \
                                     windows do, so nothing stored there can be read and the \
                                     data lives in memory until the tab closes ({why})"
                                )),
                            );
                        }
                    }
                }
                refusals += 1;
            } else {
                refusals = 0;
            }
        }
        // The worker went away while the files opened: close them, then
        // let go of the lock.
        let (Some(tabs), Some(state)) = (tabs_ref.upgrade(), state_ref.upgrade()) else {
            drop(opened);
            drop(leadership);
            return;
        };
        let weak = Rc::downgrade(&tabs);
        let push: Push = Rc::new(move |to, body| {
            if let Some(tabs) = weak.upgrade() {
                tabs.push(to, body);
            }
        });
        let engine = opened.and_then(|opened| {
            let first_write: Option<Box<dyn FnOnce()>> = opened
                .durable
                .then(|| Box::new(|| spawn_local(ask_page_to_persist())) as Box<dyn FnOnce()>);
            Engine::new(opened, first_write, push)
        });
        if state.borrow().closed {
            // Closed while opening: let go of the files, then the lock.
            drop(engine);
            drop(leadership);
            return;
        }
        let e = match engine {
            Ok(engine) => {
                spawn_local(sync_in_background(
                    engine.downgrade(),
                    lock_name(&db),
                    tabs.tab_id().to_owned(),
                ));
                let weak = engine.downgrade();
                // A subscribe answer names this tab, the leader that made
                // the subscription: pages keep only the current leader's.
                let me = JsValue::from(tabs.tab_id());
                // Takes over from the handler that answered with the error,
                // if one did.
                tabs.lead(Box::new(move |req, from| {
                    let subscribe = js::get_str(&req, "op").as_deref() == Some("subscribe");
                    let answer = engine.handle(req, from)?;
                    if subscribe && js::get(&answer, "ok").as_bool() == Some(true) {
                        js_sys::Reflect::set(&answer, &"leader".into(), &me)?;
                    }
                    Ok(answer)
                }));
                if answer.is_some() {
                    // The tabs already know this one leads, from when it answered
                    // with the error, so the leader change above told nobody:
                    // say that the database is open now, so that pages read
                    // again and make the subscriptions it refused.
                    tabs.push(
                        None,
                        js::object(&[
                            ("type", "leader".into()),
                            ("tab", tabs.tab_id().into()),
                            ("opened", true.into()),
                        ]),
                    );
                }
                let mut s = state.borrow_mut();
                s.error = None;
                s.leadership = Some(leadership);
                s.engine = Some(weak);
                return;
            }
            Err(e) => e,
        };
        // Usually the previous leader has not let go of the files yet.
        let message = js::message(&e);
        failures += 1;
        if failures < retry.attempts {
            state.borrow_mut().error = Some(message);
            drop((leadership, tabs, state));
            js::sleep_ms(retry.delay_ms).await;
            continue;
        }
        // Keep the lock and answer with the reason, so every tab learns why
        // instead of waiting, and keep trying.
        let reason = if refused_existing {
            format!(
                "the browser refused to open the existing database {db:?}, which is kept in \
                 its storage ({message}); close other windows of this site and reload, or \
                 open it in a window that is not private"
            )
        } else {
            format!("the database could not be opened: {message}")
        };
        state.borrow_mut().error = Some(reason.clone());
        match &answer {
            Some(a) => *a.borrow_mut() = reason,
            None => {
                let a = Rc::new(RefCell::new(reason));
                let said = a.clone();
                tabs.lead(Box::new(move |_, _| Err(js::error(&said.borrow()))));
                answer = Some(a);
            }
        }
        // Between attempts the lock waits where `close`, or dropping the
        // worker, lets go of it.
        state.borrow_mut().leadership = Some(leadership);
        drop((tabs, state));
        let doublings = (failures - retry.attempts).min(16);
        js::sleep_ms(
            retry
                .delay_ms
                .saturating_mul(1 << doublings)
                .min(retry.max_delay_ms),
        )
        .await;
        let Some(state) = state_ref.upgrade() else {
            // The worker went away, and the lock with its state.
            return;
        };
        kept = state.borrow_mut().leadership.take();
        if kept.is_none() {
            // Closed meanwhile, which released the lock.
            return;
        }
    }
}

/// Pushes and pulls about once a second while the engine is open, and
/// every ten seconds ends the subscriptions of tabs that closed. Holds the
/// engine only during a round, so closing it is not held up between rounds,
/// and stops once the worker closes the engine.
async fn sync_in_background(engine: WeakEngine, channel: String, me: String) {
    let mut round: u32 = 0;
    loop {
        js::sleep_ms(1_000).await;
        let Some(engine) = engine.upgrade() else {
            return;
        };
        if engine.closing() {
            return;
        }
        engine.tick().await;
        round = round.wrapping_add(1);
        if round.is_multiple_of(10)
            && engine.subscription_count() > 0
            && let Ok(mut live) = tabs::live_tabs(&channel).await
        {
            live.insert(me.clone());
            engine.prune(&live);
        }
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
