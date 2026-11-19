//! Effects: mail, webhook calls and jobs that writes leave behind as rows,
//! for a host that can reach the outside world to carry out.
//!
//! A write records what it owes in the writer-only store, in the same
//! transaction as the change, so a mail or a call is owed exactly when the
//! change is kept, and only the writer, which alone holds that store, ever
//! sends it. A tab keeps them (its inbox shows the mail); the native server
//! takes them in batches, sends them and reports back, through these calls
//! on [`Engine`](crate::Engine):
//!
//! | Rows | Taken by | Reported by |
//! | --- | --- | --- |
//! | Mail in `_inbox` | `due_mail` | `mail_done` |
//! | Webhook calls in `_outbox` | `due_webhooks` | `webhook_done` |
//! | Jobs in `_workers` | `claim_jobs` | `job_done` |
//!
//! Taking a row leases it: if the host stops before it reports, the row is
//! due again once the lease ends, so an effect can happen twice after a
//! crash but is never lost. Failures are retried with backoff, up to a
//! limit; mail older than [`MAIL_MAX_AGE_MS`] is not sent at all, because
//! its code or link has expired by then.

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::time;

/// How long a host has to report on an effect it took.
pub const LEASE_MS: i64 = 5 * 60 * 1000;

/// Mail not sent within this long after it was written is dropped: sign-in
/// codes last ten minutes and reset links thirty.
pub const MAIL_MAX_AGE_MS: i64 = 30 * 60 * 1000;

/// How many times mail is tried.
pub const MAIL_ATTEMPTS: i64 = 5;

/// How many times a webhook call is tried, over about two hours.
pub const WEBHOOK_ATTEMPTS: i64 = 8;

/// How long sent and failed effects, and finished jobs, are kept.
pub const KEEP_MS: i64 = 7 * 24 * 3600 * 1000;

/// One mail to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mail {
    /// Its id in the inbox.
    pub id: String,
    /// The auth collection it is for.
    pub collection: String,
    /// The recipient.
    pub to: String,
    /// What it is: `otp`, `password_reset`, `test`, …
    pub kind: String,
    /// The subject.
    pub subject: String,
    /// The body, which templates write as HTML.
    pub body: String,
    /// This attempt's number, from 1.
    pub attempt: i64,
}

/// One webhook call to make.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebhookCall {
    /// Its id in the outbox.
    pub id: String,
    /// The collection's name now.
    pub collection: String,
    /// The webhook's id.
    pub webhook: String,
    /// Where to send it.
    pub url: String,
    /// `create:after`, `update:after` or `delete:after`.
    pub event: String,
    /// The JSON body, exactly as it is to be sent and signed.
    pub payload: String,
    /// The webhook's secret, for the signature.
    pub secret: Option<String>,
    /// When the change happened, RFC 3339.
    pub timestamp: String,
    /// This attempt's number, from 1.
    pub attempt: i64,
}

/// Seconds to wait before attempt `n + 1`: 30 s, doubling, at most an hour.
fn backoff_ms(n: i64) -> i64 {
    (30_000i64 << (n - 1).clamp(0, 10)).min(3_600_000)
}

pub(crate) fn due_mail(
    conn: &Connection,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<Mail>> {
    let now = time::rfc3339(now_ms);
    let oldest = time::rfc3339(now_ms - MAIL_MAX_AGE_MS);
    conn.execute(
        "UPDATE writer._inbox SET status = 'expired'
         WHERE (status IS NULL OR status IN ('retry', 'sending')) AND createdAt < ?1",
        [&oldest],
    )?;
    let mut stmt = conn.prepare(
        "SELECT id, collection, recipient, kind, subject, body, attempts FROM writer._inbox
         WHERE (status IS NULL OR (status IN ('retry', 'sending') AND nextAt <= ?1))
         ORDER BY createdAt, id LIMIT ?2",
    )?;
    let rows: Vec<Mail> = stmt
        .query_map(params![now, limit as i64], |r| {
            Ok(Mail {
                id: r.get(0)?,
                collection: r.get(1)?,
                to: r.get(2)?,
                kind: r.get(3)?,
                subject: r.get(4)?,
                body: r.get(5)?,
                attempt: r.get::<_, i64>(6)? + 1,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    let lease = time::rfc3339(now_ms + LEASE_MS);
    for m in &rows {
        conn.execute(
            "UPDATE writer._inbox SET status = 'sending', attempts = attempts + 1, nextAt = ?1 WHERE id = ?2",
            params![lease, m.id],
        )?;
    }
    Ok(rows)
}

pub(crate) fn mail_done(
    conn: &Connection,
    id: &str,
    outcome: Result<(), String>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let now = time::rfc3339(now_ms);
    match outcome {
        Ok(()) => conn.execute(
            "UPDATE writer._inbox SET status = 'sent', sentAt = ?1, error = NULL WHERE id = ?2",
            params![now, id],
        )?,
        Err(e) => {
            let attempts: i64 = conn
                .query_row(
                    "SELECT attempts FROM writer._inbox WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?
                .unwrap_or(MAIL_ATTEMPTS);
            if attempts >= MAIL_ATTEMPTS {
                conn.execute(
                    "UPDATE writer._inbox SET status = 'failed', error = ?1 WHERE id = ?2",
                    params![e, id],
                )?
            } else {
                conn.execute(
                    "UPDATE writer._inbox SET status = 'retry', error = ?1, nextAt = ?2 WHERE id = ?3",
                    params![e, time::rfc3339(now_ms + backoff_ms(attempts)), id],
                )?
            }
        }
    };
    Ok(())
}

pub(crate) fn due_webhooks(
    conn: &Connection,
    now_ms: i64,
    limit: usize,
) -> rusqlite::Result<Vec<WebhookCall>> {
    let now = time::rfc3339(now_ms);
    let mut stmt = conn.prepare(
        "SELECT o.id, coalesce(m.name, ''), o.collection, o.target, o.url, o.event, o.payload,
                o.createdAt, o.attempts
         FROM writer._outbox o LEFT JOIN main._moul m ON m.id = o.collection
         WHERE o.kind = 'webhook' AND o.status IN ('pending', 'retry', 'sending') AND o.nextAt <= ?1
         ORDER BY o.createdAt, o.id LIMIT ?2",
    )?;
    let rows: Vec<(WebhookCall, String)> = stmt
        .query_map(params![now, limit as i64], |r| {
            Ok((
                WebhookCall {
                    id: r.get(0)?,
                    collection: r.get(1)?,
                    webhook: r.get(3)?,
                    url: r.get(4)?,
                    event: r.get(5)?,
                    payload: r.get(6)?,
                    secret: None,
                    timestamp: r.get(7)?,
                    attempt: r.get::<_, i64>(8)? + 1,
                },
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let lease = time::rfc3339(now_ms + LEASE_MS);
    let mut out = Vec::with_capacity(rows.len());
    for (mut call, collection_id) in rows {
        // A webhook removed or switched off since the change owes nothing.
        if !webhook_live(conn, &collection_id, &call.webhook)? {
            conn.execute(
                "UPDATE writer._outbox SET status = 'failed', lastError = 'the webhook was removed or switched off' WHERE id = ?1",
                [&call.id],
            )?;
            continue;
        }
        call.secret = crate::webhooks::secret(conn, &collection_id, &call.webhook);
        conn.execute(
            "UPDATE writer._outbox SET status = 'sending', attempts = attempts + 1, nextAt = ?1 WHERE id = ?2",
            params![lease, call.id],
        )?;
        out.push(call);
    }
    Ok(out)
}

/// Whether webhook `id` of the collection still exists and is enabled.
fn webhook_live(conn: &Connection, collection_id: &str, id: &str) -> rusqlite::Result<bool> {
    let hooks: Option<String> = conn
        .query_row(
            "SELECT webhooks FROM main._moul WHERE id = ?1",
            [collection_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let hooks: Vec<Value> = hooks
        .and_then(|h| serde_json::from_str(&h).ok())
        .unwrap_or_default();
    Ok(hooks
        .iter()
        .any(|h| h["id"].as_str() == Some(id) && h["enabled"].as_bool().unwrap_or(false)))
}

pub(crate) fn webhook_done(
    conn: &Connection,
    id: &str,
    outcome: Result<(), String>,
    now_ms: i64,
) -> rusqlite::Result<()> {
    let now = time::rfc3339(now_ms);
    match outcome {
        Ok(()) => conn.execute(
            "UPDATE writer._outbox SET status = 'sent', sentAt = ?1, lastError = NULL WHERE id = ?2",
            params![now, id],
        )?,
        Err(e) => {
            let attempts: i64 = conn
                .query_row("SELECT attempts FROM writer._outbox WHERE id = ?1", [id], |r| r.get(0))
                .optional()?
                .unwrap_or(WEBHOOK_ATTEMPTS);
            if attempts >= WEBHOOK_ATTEMPTS {
                conn.execute(
                    "UPDATE writer._outbox SET status = 'failed', lastError = ?1 WHERE id = ?2",
                    params![e, id],
                )?
            } else {
                conn.execute(
                    "UPDATE writer._outbox SET status = 'retry', lastError = ?1, nextAt = ?2 WHERE id = ?3",
                    params![e, time::rfc3339(now_ms + backoff_ms(attempts)), id],
                )?
            }
        }
    };
    Ok(())
}

/// Deletes what is done and older than [`KEEP_MS`]: sent, failed and
/// expired mail and calls, and finished jobs.
pub(crate) fn prune(conn: &Connection, now_ms: i64) -> rusqlite::Result<usize> {
    let cutoff = time::rfc3339(now_ms - KEEP_MS);
    let mut n = conn.execute(
        "DELETE FROM writer._outbox WHERE status IN ('sent', 'failed') AND createdAt < ?1",
        [&cutoff],
    )?;
    n += conn.execute(
        "DELETE FROM writer._inbox WHERE status IN ('sent', 'failed', 'expired') AND createdAt < ?1",
        [&cutoff],
    )?;
    n += crate::workers::prune(conn, KEEP_MS, now_ms)?;
    Ok(n)
}

/// What is waiting, for a status line: mail, webhook calls and jobs that
/// are due or being sent.
pub(crate) fn pending(conn: &Connection) -> rusqlite::Result<Value> {
    let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
    Ok(serde_json::json!({
        "mail": count("SELECT count(*) FROM writer._inbox WHERE status IS NULL OR status IN ('retry', 'sending')")?,
        "webhooks": count("SELECT count(*) FROM writer._outbox WHERE status IN ('pending', 'retry', 'sending')")?,
        "jobs": count("SELECT count(*) FROM writer._workers WHERE state IN ('available', 'executing')")?,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_an_hour() {
        assert_eq!(backoff_ms(1), 30_000);
        assert_eq!(backoff_ms(2), 60_000);
        assert_eq!(backoff_ms(8), 3_600_000);
        assert_eq!(backoff_ms(40), 3_600_000);
    }
}
