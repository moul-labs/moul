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
//! | `push`     | `to`?, `body`              | From the leader to tab `to`, or to all    |
//!
//! Each tab also holds a Web Lock named `<channel>:tab:<id>` for as long as
//! it lives, so the leader can tell which tabs are gone with
//! [`live_tabs`] and drop what it kept for them.
//!
//! Whatever reaches a tab (pushes, and `{ type: "leader", tab }` when the
//! leading tab changes) goes to the listener set with
//! [`Tabs::on_push`]; the worker posts it to its page. The listener is
//! called on a microtask, in the order the messages arrived, never while
//! the leader is answering a request: it may call back into the tab (ask
//! whether it leads, send a request, close it).
//!
//! A request in flight when its leader goes away fails with
//! [`LEADER_LOST`], and the caller decides whether to send it again. Phase 4
//! gives every write a request id, which makes sending it again safe.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
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

/// Answers requests while this tab leads, given the request and the id of
/// the tab that sent it. A handler that answers later returns a `Promise`,
/// which settles the request when it settles.
pub type Handler = Box<dyn FnMut(JsValue, &str) -> Result<JsValue, JsValue>>;

/// A request waiting for its answer.
struct Pending {
    resolve: Function,
    reject: Function,
    /// The leader it was sent to; `None` while it waits for a leader.
    sent_to: Option<String>,
}

/// What waits for this tab's listener, delivered in order on a microtask.
#[derive(Default)]
struct Outbox {
    queue: RefCell<VecDeque<JsValue>>,
    scheduled: Cell<bool>,
    /// Receives what reaches this tab: pushes and leader changes.
    listener: RefCell<Option<Function>>,
}

impl Outbox {
    /// Hands everything queued to the listener, including what it queues
    /// while it runs.
    fn flush(&self) {
        loop {
            let next = self.queue.borrow_mut().pop_front();
            let Some(body) = next else {
                break;
            };
            let listener = self.listener.borrow().clone();
            if let Some(f) = listener {
                let _ = f.call1(&JsValue::NULL, &body);
            }
        }
        self.scheduled.set(false);
    }
}

struct Inner {
    channel: BroadcastChannel,
    me: String,
    leader: RefCell<Option<String>>,
    handler: RefCell<Option<Handler>>,
    /// Whether this tab leads. Kept apart from `handler`, which is borrowed
    /// while it answers.
    leading: Cell<bool>,
    pending: RefCell<HashMap<u64, Pending>>,
    queued: RefCell<Vec<(u64, JsValue)>>,
    next: Cell<u64>,
    /// Set by [`Tabs::disconnect`]: leave without a word.
    silent: Cell<bool>,
    outbox: Rc<Outbox>,
    /// This tab's liveness lock, once granted.
    alive: RefCell<Option<crate::leader::Leadership>>,
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
        self.leading.get()
    }

    fn handle(&self, body: JsValue, from: &str) -> Result<JsValue, JsValue> {
        // Nothing this tab runs while the handler answers calls back into
        // it (the listener waits for a microtask), so the handler is free.
        let Ok(mut handler) = self.handler.try_borrow_mut() else {
            return Err(js::error(
                "the leading tab was asked again while it was answering a request",
            ));
        };
        match handler.as_mut() {
            Some(h) => h(body, from),
            None => Err(js::error("this tab does not lead")),
        }
    }

    /// Queues `body` for this tab's listener, which gets it on a microtask
    /// after everything queued before it.
    fn deliver(&self, body: &JsValue) {
        self.outbox.queue.borrow_mut().push_back(body.clone());
        if !self.outbox.scheduled.replace(true) {
            let outbox = self.outbox.clone();
            wasm_bindgen_futures::spawn_local(async move { outbox.flush() });
        }
    }

    /// Records that `tab` leads, and tells this tab's listener if that is a
    /// change.
    fn set_leader(&self, tab: &str) {
        let changed = self.leader.borrow().as_deref() != Some(tab);
        *self.leader.borrow_mut() = Some(tab.to_owned());
        if changed {
            self.deliver(&js::object(&[
                ("type", "leader".into()),
                ("tab", tab.into()),
            ]));
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
                let result = self.handle(body, &self.me);
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
                self.set_leader(&tab);
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
                let result = self.handle(js::get(&msg, "body"), &from);
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
            "push" => {
                let to = js::get_str(&msg, "to");
                if to.is_none() || to.as_deref() == Some(self.me.as_str()) {
                    self.deliver(&js::get(&msg, "body"));
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
            leading: Cell::new(false),
            pending: RefCell::new(HashMap::new()),
            queued: RefCell::new(Vec::new()),
            next: Cell::new(1),
            silent: Cell::new(false),
            outbox: Rc::new(Outbox::default()),
            alive: RefCell::new(None),
        });
        let lock = format!("{}{}", tab_lock_prefix(channel), inner.me);
        let holder = Rc::downgrade(&inner);
        wasm_bindgen_futures::spawn_local(async move {
            if let Ok(held) = crate::leader::Leadership::acquire(&lock).await
                && let Some(inner) = holder.upgrade()
            {
                *inner.alive.borrow_mut() = Some(held);
            }
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

    /// Sets the function that receives what reaches this tab: pushes from
    /// the leader, and `{ type: "leader", tab }` when the leader changes.
    pub fn on_push(&self, listener: Option<Function>) {
        *self.inner.outbox.listener.borrow_mut() = listener;
    }

    /// Sends `body` from the leader to tab `to`, or to every tab with
    /// `None`. This tab's own listener gets it on a microtask, after what
    /// reached it before.
    pub fn push(&self, to: Option<&str>, body: JsValue) {
        let me = self.inner.me.as_str();
        if to.is_none_or(|t| t == me) {
            self.inner.deliver(&body);
        }
        if to != Some(me) {
            self.inner.post(js::object(&[
                ("type", "push".into()),
                ("to", to.map_or(JsValue::NULL, JsValue::from)),
                ("body", body),
            ]));
        }
    }

    /// Makes this tab the leader: `handler` answers every request from now on,
    /// including this tab's own. Call it only while holding the leader lock.
    pub fn lead(&self, handler: Handler) {
        *self.inner.handler.borrow_mut() = Some(handler);
        self.inner.leading.set(true);
        let me = self.inner.me.clone();
        self.inner.set_leader(&me);
        self.inner.fail_sent(Some(&self.inner.me));
        self.inner.announce();
        self.inner.flush_queue();
    }

    /// Stops leading and tells the other tabs. Returns the handler.
    pub fn resign(&self) -> Option<Handler> {
        self.inner.leading.set(false);
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
            let result = self.inner.handle(body, &self.inner.me);
            // A handler that answers later returns a promise.
            return match result {
                Ok(v) if v.is_instance_of::<js_sys::Promise>() => {
                    JsFuture::from(v.unchecked_into::<js_sys::Promise>()).await
                }
                other => other,
            };
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
        // Nothing reaches a tab that left.
        self.inner.outbox.queue.borrow_mut().clear();
        *self.inner.outbox.listener.borrow_mut() = None;
        self.inner.channel.set_onmessage(None);
        self.inner.channel.close();
    }
}

/// The prefix of the liveness locks of the tabs on `channel`.
fn tab_lock_prefix(channel: &str) -> String {
    format!("{channel}:tab:")
}

/// The ids of the tabs on `channel` that are still open: those whose
/// liveness lock is held.
pub async fn live_tabs(channel: &str) -> Result<std::collections::HashSet<String>, JsValue> {
    let locks = js::get(&js::navigator(), "locks");
    if locks.is_undefined() {
        return Err(js::error("the Web Locks API is not available"));
    }
    let snapshot = js::call_async(&locks, "query", &[]).await?;
    let prefix = tab_lock_prefix(channel);
    let held = js::get(&snapshot, "held");
    let mut out = std::collections::HashSet::new();
    if js_sys::Array::is_array(&held) {
        for lock in js_sys::Array::from(&held).iter() {
            if let Some(id) =
                js::get_str(&lock, "name").and_then(|n| n.strip_prefix(&prefix).map(str::to_owned))
            {
                out.insert(id);
            }
        }
    }
    Ok(out)
}
