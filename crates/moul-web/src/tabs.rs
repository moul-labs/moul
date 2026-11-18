//! Requests between tabs: every tab sends, the leader answers.
//!
//! Each tab's worker joins a broadcast channel named after the database. The
//! leader answers requests addressed to it; the others forward theirs. Messages:
//!
//! | `type`     | Fields                    | Meaning                                   |
//! | ---------- | ------------------------- | ----------------------------------------- |
//! | `who`      | `from`                    | A tab joined and asks who leads           |
//! | `leader`   | `tab`                     | `tab` leads now                           |
//! | `resign`   | `tab`                     | `tab` stopped leading                     |
//! | `request`  | `from`, `to`, `id`, `body` | A request for the leader `to`             |
//! | `response` | `to`, `id`, `ok`, `body`   | The answer, or an error message           |
//!
//! A request in flight when its leader goes away fails with
//! [`LEADER_LOST`], and the caller decides whether to send it again. Phase 4
//! gives every write a request id, which makes sending it again safe.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use js_sys::Function;
use wasm_bindgen::closure::Closure;
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use web_sys::{BroadcastChannel, MessageEvent};

use crate::js;

/// The message of the error a request fails with when its leader goes away.
pub const LEADER_LOST: &str = "the leading tab went away before answering";

/// Answers requests while this tab leads. A handler that answers later
/// returns a `Promise`, which settles the request when it settles.
pub type Handler = Box<dyn FnMut(JsValue) -> Result<JsValue, JsValue>>;

/// A request waiting for its answer.
struct Pending {
    resolve: Function,
    reject: Function,
    /// The leader it was sent to; `None` while it waits for a leader.
    sent_to: Option<String>,
}

struct Inner {
    channel: BroadcastChannel,
    me: String,
    leader: RefCell<Option<String>>,
    handler: RefCell<Option<Handler>>,
    pending: RefCell<HashMap<u64, Pending>>,
    queued: RefCell<Vec<(u64, JsValue)>>,
    next: Cell<u64>,
    /// Set by [`Tabs::disconnect`]: leave without a word.
    silent: Cell<bool>,
}

/// This tab's place among the tabs of one database.
pub struct Tabs {
    inner: Rc<Inner>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
}

impl fmt::Debug for Tabs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tabs")
            .field("me", &self.inner.me)
            .field("leader", &self.inner.leader.borrow())
            .finish_non_exhaustive()
    }
}

fn new_tab_id() -> String {
    let crypto = js::get(&js::global(), "crypto");
    js::call(&crypto, "randomUUID", &[])
        .ok()
        .and_then(|v| v.as_string())
        .unwrap_or_else(|| format!("tab-{}", js_sys::Math::random().to_bits()))
}

impl Inner {
    fn post(&self, msg: JsValue) {
        let _ = self.channel.post_message(&msg);
    }

    fn send_request(&self, to: &str, id: u64, body: &JsValue) {
        self.post(js::object(&[
            ("type", "request".into()),
            ("from", self.me.as_str().into()),
            ("to", to.into()),
            ("id", (id as f64).into()),
            ("body", body.clone()),
        ]));
    }

    fn is_leader(&self) -> bool {
        self.handler.borrow().is_some()
    }

    fn handle(&self, body: JsValue) -> Result<JsValue, JsValue> {
        match self.handler.borrow_mut().as_mut() {
            Some(h) => h(body),
            None => Err(js::error("this tab does not lead")),
        }
    }

    /// Fails every request already sent to a leader other than `keep`.
    /// Requests still waiting for a leader stay queued.
    fn fail_sent(&self, keep: Option<&str>) {
        let mut lost = Vec::new();
        self.pending
            .borrow_mut()
            .retain(|_, p| match p.sent_to.as_deref() {
                Some(to) if Some(to) != keep => {
                    lost.push(p.reject.clone());
                    false
                }
                _ => true,
            });
        for reject in lost {
            let _ = reject.call1(&JsValue::NULL, &js::error(LEADER_LOST));
        }
    }

    /// Fails everything, sent or queued. For shutting down.
    fn fail_all(&self) {
        self.queued.borrow_mut().clear();
        for (_, p) in self.pending.borrow_mut().drain() {
            let _ = p.reject.call1(&JsValue::NULL, &js::error(LEADER_LOST));
        }
    }

    /// Settles queued requests now that a leader is known.
    fn flush_queue(&self) {
        let queued = std::mem::take(&mut *self.queued.borrow_mut());
        for (id, body) in queued {
            if self.is_leader() {
                let result = self.handle(body);
                if let Some(p) = self.pending.borrow_mut().remove(&id) {
                    let _ = match result {
                        Ok(v) => p.resolve.call1(&JsValue::NULL, &v),
                        Err(e) => p.reject.call1(&JsValue::NULL, &e),
                    };
                }
            } else if let Some(leader) = self.leader.borrow().clone() {
                if let Some(p) = self.pending.borrow_mut().get_mut(&id) {
                    p.sent_to = Some(leader.clone());
                }
                self.send_request(&leader, id, &body);
            } else {
                self.queued.borrow_mut().push((id, body));
            }
        }
    }

    fn on_message(&self, msg: JsValue) {
        let Some(kind) = js::get_str(&msg, "type") else {
            return;
        };
        match kind.as_str() {
            "who" if self.is_leader() => self.announce(),
            "leader" => {
                let Some(tab) = js::get_str(&msg, "tab") else {
                    return;
                };
                self.fail_sent(Some(&tab));
                *self.leader.borrow_mut() = Some(tab);
                self.flush_queue();
            }
            "resign" => {
                let tab = js::get_str(&msg, "tab");
                if tab.is_some() && *self.leader.borrow() == tab {
                    *self.leader.borrow_mut() = None;
                    self.fail_sent(None);
                }
            }
            "request" => {
                if js::get_str(&msg, "to").as_deref() != Some(self.me.as_str()) || !self.is_leader()
                {
                    return;
                }
                let from = js::get_str(&msg, "from").unwrap_or_default();
                let id = js::get(&msg, "id");
                let result = self.handle(js::get(&msg, "body"));
                let channel = self.channel.clone();
                let reply = move |result: Result<JsValue, JsValue>| {
                    let msg = match result {
                        Ok(body) => js::object(&[
                            ("type", "response".into()),
                            ("to", from.into()),
                            ("id", id),
                            ("ok", true.into()),
                            ("body", body),
                        ]),
                        Err(e) => js::object(&[
                            ("type", "response".into()),
                            ("to", from.into()),
                            ("id", id),
                            ("ok", false.into()),
                            ("body", js::message(&e).into()),
                        ]),
                    };
                    let _ = channel.post_message(&msg);
                };
                // A handler answers later by returning a promise; a promise
                // cannot cross the channel, so wait for it here.
                match result {
                    Ok(v) if v.is_instance_of::<js_sys::Promise>() => {
                        let promise: js_sys::Promise = v.unchecked_into();
                        wasm_bindgen_futures::spawn_local(async move {
                            reply(JsFuture::from(promise).await);
                        });
                    }
                    other => reply(other),
                }
            }
            "response" => {
                if js::get_str(&msg, "to").as_deref() != Some(self.me.as_str()) {
                    return;
                }
                let Some(id) = js::get(&msg, "id").as_f64() else {
                    return;
                };
                let Some(p) = self.pending.borrow_mut().remove(&(id as u64)) else {
                    return;
                };
                let body = js::get(&msg, "body");
                let _ = if js::get(&msg, "ok").as_bool() == Some(true) {
                    p.resolve.call1(&JsValue::NULL, &body)
                } else {
                    p.reject
                        .call1(&JsValue::NULL, &js::error(&js::message(&body)))
                };
            }
            _ => {}
        }
    }

    fn announce(&self) {
        self.post(js::object(&[
            ("type", "leader".into()),
            ("tab", self.me.as_str().into()),
        ]));
    }
}

impl Tabs {
    /// Joins the tabs of the database whose channel is `channel`, and asks
    /// who leads.
    pub fn join(channel: &str) -> Result<Tabs, JsValue> {
        let inner = Rc::new(Inner {
            channel: BroadcastChannel::new(channel)?,
            me: new_tab_id(),
            leader: RefCell::new(None),
            handler: RefCell::new(None),
            pending: RefCell::new(HashMap::new()),
            queued: RefCell::new(Vec::new()),
            next: Cell::new(1),
            silent: Cell::new(false),
        });
        let weak = Rc::downgrade(&inner);
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |e: MessageEvent| {
            if let Some(inner) = weak.upgrade() {
                inner.on_message(e.data());
            }
        });
        inner
            .channel
            .set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        inner.post(js::object(&[
            ("type", "who".into()),
            ("from", inner.me.as_str().into()),
        ]));
        Ok(Tabs {
            inner,
            _on_message: on_message,
        })
    }

    /// This tab's id.
    pub fn tab_id(&self) -> &str {
        &self.inner.me
    }

    /// The leading tab's id, if one is known.
    pub fn leader(&self) -> Option<String> {
        self.inner.leader.borrow().clone()
    }

    /// Whether this tab leads.
    pub fn is_leader(&self) -> bool {
        self.inner.is_leader()
    }

    /// Makes this tab the leader: `handler` answers every request from now on,
    /// including this tab's own. Call it only while holding the leader lock.
    pub fn lead(&self, handler: Handler) {
        *self.inner.handler.borrow_mut() = Some(handler);
        *self.inner.leader.borrow_mut() = Some(self.inner.me.clone());
        self.inner.fail_sent(Some(&self.inner.me));
        self.inner.announce();
        self.inner.flush_queue();
    }

    /// Stops leading and tells the other tabs. Returns the handler.
    pub fn resign(&self) -> Option<Handler> {
        let handler = self.inner.handler.borrow_mut().take();
        if handler.is_some() {
            *self.inner.leader.borrow_mut() = None;
            self.inner.post(js::object(&[
                ("type", "resign".into()),
                ("tab", self.inner.me.as_str().into()),
            ]));
        }
        handler
    }

    /// Sends `body` to the leader and waits for the answer. Waits for a
    /// leader if none is known yet.
    pub async fn request(&self, body: JsValue) -> Result<JsValue, JsValue> {
        if self.inner.is_leader() {
            return self.inner.handle(body);
        }
        let id = self.inner.next.get();
        self.inner.next.set(id + 1);
        let d = js::deferred();
        let leader = self.inner.leader.borrow().clone();
        self.inner.pending.borrow_mut().insert(
            id,
            Pending {
                resolve: d.resolve.clone(),
                reject: d.reject.clone(),
                sent_to: leader.clone(),
            },
        );
        match leader {
            Some(leader) => self.inner.send_request(&leader, id, &body),
            None => self.inner.queued.borrow_mut().push((id, body)),
        }
        JsFuture::from(d.promise).await
    }
}

impl Tabs {
    /// Leaves without telling the other tabs, the way a tab that crashes or is
    /// closed does: its requests in flight are never answered.
    pub fn disconnect(self) {
        self.inner.silent.set(true);
    }
}

impl Drop for Tabs {
    fn drop(&mut self) {
        if !self.inner.silent.get() {
            self.resign();
        }
        self.inner.fail_all();
        self.inner.channel.set_onmessage(None);
        self.inner.channel.close();
    }
}
