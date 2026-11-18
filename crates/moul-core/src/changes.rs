//! The change log: one row in the shared store for each write request,
//! applied or refused.
//!
//! The row is written in the same transaction as the change itself, so it
//! travels to followers in the same LTX file. Followers read it to learn
//! what changed (pages applied underneath SQLite fire no events), live
//! queries read it to refresh, and from Phase 9 devices read it to drop the
//! requests the writer confirmed or refused.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::api::{ApiResult, Response};
use crate::engine::{Catalog, Cx};
use crate::realtime::{Action, RecordEvent};
use crate::values;

/// What a write changed, as a handler reports it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChangeInfo {
    /// The collection's id.
    pub collection: Option<String>,
    /// The record's id.
    pub record: Option<String>,
    /// The field ids (or system column names) that changed.
    pub fields: Vec<String>,
    /// Other records the write changed: cascaded deletes and cleared
    /// references, as (collection id, record id, operation).
    pub effects: Vec<(String, String, String)>,
}

/// A record a write changed besides its own, such as a record a delete
/// cascaded to or one whose reference it cleared.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Effect {
    /// The collection's id.
    pub collection: String,
    /// The record's id.
    pub record: String,
    /// `delete` or `update`.
    pub op: String,
}

/// One row of the change log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Change {
    /// Position in the writer's order.
    pub seq: i64,
    /// The request id.
    pub request: String,
    /// Who sent it: `owner`, `<collection id>/<record id>`, or empty for a guest.
    pub actor: String,
    /// `create`, `update`, `delete`, `define`, `undefine`, `schema`,
    /// `import`, `upload` or `unupload`.
    pub op: String,
    /// The collection's id.
    pub collection: Option<String>,
    /// The record's id.
    pub record: Option<String>,
    /// Field ids, or system column names, that changed.
    pub fields: Vec<String>,
    /// The HTTP status of the answer: below 400 applied, otherwise refused.
    pub status: u16,
    /// Why it was refused.
    pub error: Option<String>,
    /// When the writer applied it, RFC 3339.
    pub at: String,
    /// Other records the write changed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
}

impl Change {
    /// Whether the request was applied.
    pub fn applied(&self) -> bool {
        self.status < 400
    }
}

/// Appends a row and returns its sequence number.
#[allow(clippy::too_many_arguments)]
pub(crate) fn record(
    conn: &Connection,
    request: &str,
    actor: &str,
    op: &str,
    info: &ChangeInfo,
    status: u16,
    error: Option<&str>,
    at: &str,
) -> ApiResult<i64> {
    let fields = if info.fields.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&info.fields).expect("strings serialise"))
    };
    let effects = if info.effects.is_empty() {
        None
    } else {
        let list: Vec<Effect> = info
            .effects
            .iter()
            .map(|(c, r, o)| Effect {
                collection: c.clone(),
                record: r.clone(),
                op: o.clone(),
            })
            .collect();
        Some(serde_json::to_string(&list).expect("effects serialise"))
    };
    conn.execute(
        "INSERT INTO main._changes (request, actor, op, collection, record, fields, status, error, at, effects)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![request, actor, op, info.collection, info.record, fields, status, error, at, effects],
    )?;
    Ok(conn.last_insert_rowid())
}

fn read(row: &rusqlite::Row<'_>) -> rusqlite::Result<Change> {
    let fields: Option<String> = row.get(6)?;
    Ok(Change {
        seq: row.get(0)?,
        request: row.get(1)?,
        actor: row.get(2)?,
        op: row.get(3)?,
        collection: row.get(4)?,
        record: row.get(5)?,
        fields: fields
            .and_then(|f| serde_json::from_str(&f).ok())
            .unwrap_or_default(),
        status: row.get(7)?,
        error: row.get(8)?,
        at: row.get(9)?,
        effects: row
            .get::<_, Option<String>>(10)?
            .and_then(|e| serde_json::from_str(&e).ok())
            .unwrap_or_default(),
    })
}

const COLUMNS: &str =
    "seq, request, actor, op, collection, record, fields, status, error, at, effects";

/// The row of request `id`, if it was seen.
pub(crate) fn find(conn: &Connection, id: &str) -> ApiResult<Option<Change>> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM main._changes WHERE request = ?1"),
            [id],
            read,
        )
        .optional()?)
}

/// Rows after `seq`, oldest first.
pub(crate) fn since(conn: &Connection, seq: i64, limit: usize) -> ApiResult<Vec<Change>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM main._changes WHERE seq > ?1 ORDER BY seq LIMIT ?2"
    ))?;
    let rows = stmt
        .query_map(params![seq, limit as i64], read)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The newest `n` rows of the change log, oldest first.
fn tail(conn: &Connection, n: usize) -> ApiResult<Vec<Change>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT * FROM (SELECT {COLUMNS} FROM main._changes ORDER BY seq DESC LIMIT ?1) ORDER BY seq"
    ))?;
    let rows = stmt
        .query_map(params![n as i64], read)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The record events a follower announces for changes it received: the
/// current state of each record that was created or updated, and a bare
/// id for each that was deleted.
pub(crate) fn events_for(
    conn: &Connection,
    catalog: &Catalog,
    changes: &[Change],
) -> Vec<RecordEvent> {
    let mut out = Vec::new();
    for ch in changes.iter().filter(|c| c.applied()) {
        let own = match (&ch.collection, &ch.record) {
            (Some(cid), Some(rid)) => Some((cid.clone(), rid.clone(), ch.op.clone())),
            _ => None,
        };
        let touched = own.into_iter().chain(
            ch.effects
                .iter()
                .map(|e| (e.collection.clone(), e.record.clone(), e.op.clone())),
        );
        for (cid, rid, op) in touched {
            if let Some(ev) = event_for(conn, catalog, &cid, &rid, &op) {
                out.push(ev);
            }
        }
    }
    out
}

/// The event for one record a logged change touched: its current state
/// for a create or update, a bare id for a delete.
fn event_for(
    conn: &Connection,
    catalog: &Catalog,
    cid: &str,
    rid: &str,
    op: &str,
) -> Option<RecordEvent> {
    let c = catalog.by_id(cid)?;
    let action = match op {
        "create" => Action::Create,
        "update" => Action::Update,
        "delete" => Action::Delete,
        _ => return None,
    };
    let record = if action == Action::Delete {
        let mut m = serde_json::Map::new();
        m.insert("id".into(), Value::String(rid.to_owned()));
        m
    } else {
        let mut r = crate::records::load(conn, c, rid).ok().flatten()?;
        values::normalize(c, &mut r);
        r
    };
    Some(RecordEvent {
        action,
        collection: c.clone(),
        record,
        old: None,
        from_log: true,
    })
}

/// `GET /api/changes?after=<seq>&limit=<n>`: the change log, owner only.
/// With `tail=<n>` instead, the newest `n` rows, oldest first.
pub(crate) fn endpoint(cx: &mut Cx<'_>) -> ApiResult {
    cx.require_admin()?;
    let after = cx.req.query_param("after").parse().unwrap_or(0);
    let limit = cx
        .req
        .query_param("limit")
        .parse::<usize>()
        .unwrap_or(500)
        .clamp(1, 5000);
    let items = match cx.req.query_param("tail").parse::<usize>() {
        Ok(n) => tail(cx.conn, n.clamp(1, 5000))?,
        Err(_) => since(cx.conn, after, limit)?,
    };
    let last = items.last().map(|c| c.seq).unwrap_or(after);
    Ok(Response::json(200, json!({"items": items, "last": last})))
}
