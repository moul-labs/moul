//! Live subscriptions inside the engine. Port of the Go engine's realtime
//! hub, without the transport: the host carries each [`Delivery`] to the
//! page, another tab, or (on the native server) a server-sent event stream.
//!
//! A subscriber gets a change when it passes the collection's subscribe
//! rule (or, without one, the view rule) as the subscriber. An update that
//! makes a record invisible to a subscriber arrives as a delete carrying the
//! last version they could see.

use std::collections::BTreeMap;

use moul_store::schema::Collection;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::api::{ApiError, Response};
use crate::engine::{Catalog, Who};
use crate::guard::SqlGuard;
use crate::{files, records, rules, time, values};

/// What happened to a record.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    /// Created.
    Create,
    /// Changed.
    Update,
    /// Deleted.
    Delete,
}

impl Action {
    fn as_str(self) -> &'static str {
        match self {
            Action::Create => "create",
            Action::Update => "update",
            Action::Delete => "delete",
        }
    }
}

/// A change a write made, before it is filtered for subscribers.
#[derive(Clone, Debug)]
pub(crate) struct RecordEvent {
    pub action: Action,
    pub collection: Collection,
    pub record: values::Record,
    pub old: Option<values::Record>,
    /// Built from the change log on a follower: a delete carries only the
    /// id, and an update no previous version.
    pub from_log: bool,
}

/// What a subscriber receives, in the Go engine's event shape.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Event {
    /// `create`, `update` or `delete`.
    pub action: Action,
    /// The collection's name.
    pub moul: String,
    /// The record as this subscriber may see it.
    pub record: Map<String, Value>,
    /// The previous version, for updates.
    #[serde(rename = "old_record", skip_serializing_if = "Option::is_none")]
    pub old_record: Option<Map<String, Value>>,
    /// When it was announced, RFC 3339.
    pub timestamp: String,
}

/// A subscription's id.
pub type SubscriptionId = u64;

/// One event for one subscription.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Delivery {
    /// The subscription it is for.
    pub subscription: SubscriptionId,
    /// The event.
    pub event: Event,
}

/// What to subscribe to, and as whom.
#[derive(Clone, Debug, Default)]
pub struct Subscription {
    /// The collection's name, or `*` for every collection.
    pub collection: String,
    /// Only this record, if set.
    pub record: Option<String>,
    /// Which actions: `create`, `update`, `delete`, or `*`. Empty means all.
    pub events: Vec<String>,
    /// The subscriber's credentials, as request headers (`Authorization`).
    pub credentials: Vec<(String, String)>,
    /// The host vouches that the subscriber is the owner.
    pub owner: bool,
}

#[derive(Debug)]
struct Subscriber {
    spec: Subscription,
    who: Who,
    /// Ids of records this subscriber was shown and not told to drop. On
    /// a follower, a delete by id goes only to those who had the record.
    visible: std::collections::HashSet<String>,
}

impl Subscriber {
    fn wants(&self, action: Action) -> bool {
        self.spec.events.is_empty()
            || self
                .spec
                .events
                .iter()
                .any(|e| e == "*" || e == "all" || e.eq_ignore_ascii_case(action.as_str()))
    }
}

/// The subscriptions of one engine and what is waiting for them.
#[derive(Debug, Default)]
pub(crate) struct Realtime {
    next: SubscriptionId,
    subs: BTreeMap<SubscriptionId, Subscriber>,
    pending: Vec<Delivery>,
    closed: Vec<(SubscriptionId, String)>,
}

impl Realtime {
    pub(crate) fn subscribe(
        &mut self,
        spec: Subscription,
        who: Who,
        catalog: &Catalog,
    ) -> Result<SubscriptionId, Response> {
        if spec.collection != "*" && catalog.get(&spec.collection).is_none() {
            return Err(ApiError::not_found("Collection not found").into_response());
        }
        if spec.collection == "*" && !who.is_admin() {
            return Err(ApiError::forbidden("Root or admin access required").into_response());
        }
        self.next += 1;
        self.subs.insert(
            self.next,
            Subscriber {
                spec,
                who,
                visible: Default::default(),
            },
        );
        Ok(self.next)
    }

    pub(crate) fn unsubscribe(&mut self, id: SubscriptionId) -> bool {
        self.subs.remove(&id).is_some()
    }

    pub(crate) fn take(&mut self) -> Vec<Delivery> {
        std::mem::take(&mut self.pending)
    }

    /// Records the host already showed a subscriber, such as the rows of
    /// the list it loaded when it subscribed.
    pub(crate) fn note_visible(&mut self, id: SubscriptionId, records: &[String]) -> bool {
        match self.subs.get_mut(&id) {
            Some(sub) => {
                sub.visible.extend(records.iter().cloned());
                true
            }
            None => false,
        }
    }

    pub(crate) fn take_closed(&mut self) -> Vec<(SubscriptionId, String)> {
        std::mem::take(&mut self.closed)
    }

    /// Ends every subscription for which `ended` gives a reason.
    pub(crate) fn retain(&mut self, mut ended: impl FnMut(&Subscription, &Who) -> Option<String>) {
        let closed = &mut self.closed;
        self.subs
            .retain(|&id, sub| match ended(&sub.spec, &sub.who) {
                Some(reason) => {
                    closed.push((id, reason));
                    false
                }
                None => true,
            });
    }

    /// Filters `events` for every subscriber and queues the deliveries,
    /// stamped with `now_ms`. File URLs are signed for subscribers who
    /// could not load them otherwise.
    pub(crate) fn publish_at(
        &mut self,
        conn: &Connection,
        catalog: &Catalog,
        guard: &SqlGuard,
        signer: &files::Signer<'_>,
        events: Vec<RecordEvent>,
        now_ms: Option<i64>,
    ) {
        if self.subs.is_empty() {
            return;
        }
        let timestamp = now_ms.map(time::rfc3339).unwrap_or_default();
        let now = now_ms.unwrap_or(0);
        for ev in events {
            let c = &ev.collection;
            // As in the Go engine: the subscribe rule, else the view rule,
            // else the list rule.
            let rule = [
                &c.rules.subscribe_rule,
                &c.rules.view_rule,
                &c.rules.list_rule,
            ]
            .into_iter()
            .find(|r| !r.is_empty())
            .unwrap_or(&c.rules.view_rule);
            for (&id, sub) in self.subs.iter_mut() {
                if sub.spec.collection != "*" && sub.spec.collection != c.name {
                    continue;
                }
                if !sub.wants(ev.action)
                    && !(ev.action == Action::Update && sub.wants(Action::Delete))
                {
                    continue;
                }
                if let Some(rid) = &sub.spec.record
                    && ev.record.get("id").and_then(Value::as_str) != Some(rid.as_str())
                {
                    continue;
                }
                let admin = sub.who.is_admin();
                let auth = sub.who.auth_map();
                let sees = |rec: &values::Record| {
                    admin
                        || rules::allows_db(
                            conn,
                            catalog,
                            guard,
                            rule,
                            auth.as_ref(),
                            rec,
                            None,
                            now,
                        )
                        .unwrap_or(false)
                };
                let id_only = |rec: &values::Record| {
                    let mut m = values::Record::new();
                    if let Some(v) = rec.get("id") {
                        m.insert("id".into(), v.clone());
                    }
                    m
                };
                let rid = ev
                    .record
                    .get("id")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_owned();
                let had = sub.visible.contains(&rid);
                let (action, record, old) = match ev.action {
                    // From the change log a delete carries only the id, and
                    // an update no previous version: only a subscriber who
                    // was shown the record is told to drop it, by id.
                    Action::Delete if ev.from_log => {
                        if !had && !admin {
                            continue;
                        }
                        (Action::Delete, ev.record.clone(), None)
                    }
                    Action::Update if ev.from_log && !sees(&ev.record) => {
                        if !had {
                            continue;
                        }
                        (Action::Delete, id_only(&ev.record), None)
                    }
                    Action::Update if !sees(&ev.record) => match &ev.old {
                        Some(old) if sees(old) => (Action::Delete, old.clone(), None),
                        _ => continue,
                    },
                    Action::Delete => {
                        let check = if ev.record.is_empty() {
                            ev.old.as_ref().unwrap_or(&ev.record)
                        } else {
                            &ev.record
                        };
                        if !sees(check) {
                            continue;
                        }
                        (Action::Delete, ev.record.clone(), ev.old.clone())
                    }
                    _ => {
                        if !sees(&ev.record) {
                            continue;
                        }
                        (ev.action, ev.record.clone(), ev.old.clone())
                    }
                };
                if action == Action::Delete {
                    sub.visible.remove(&rid);
                } else {
                    sub.visible.insert(rid.clone());
                }
                if !sub.wants(action) {
                    continue;
                }
                let clean = |mut r: values::Record| {
                    r.remove("expand");
                    files::sign_record_with(signer, conn, catalog, guard, c, &mut r);
                    records::redact(c, &mut r, auth.as_ref(), admin);
                    r
                };
                self.pending.push(Delivery {
                    subscription: id,
                    event: Event {
                        action,
                        moul: c.name.clone(),
                        record: clean(record),
                        old_record: old.map(clean),
                        timestamp: timestamp.clone(),
                    },
                });
            }
        }
    }
}
