//! Request budgets: time, SQLite steps, rows read and bytes returned.
//!
//! Every request runs under a budget so one slow query cannot stall other
//! apps on the hub. SQLite's progress handler stops a statement that runs
//! past the time or step limit; the engine counts the rows it reads and
//! measures the response before sending it.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::json;

use crate::api::ApiError;

/// Limits for one request. The engine's [`Config`](crate::Config) holds the
/// default; a request may ask for less but not more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    /// Wall-clock time in milliseconds, checked while SQLite runs. Needs a
    /// host with a statement clock; `None` turns it off.
    pub time_ms: Option<i64>,
    /// SQLite virtual machine steps, in thousands. Deterministic, so a test
    /// can replay a run exactly.
    pub ksteps: u64,
    /// Rows the engine reads from collection tables.
    pub rows: u64,
    /// Bytes of the response body.
    pub bytes: u64,
}

impl Default for Budget {
    fn default() -> Budget {
        Budget {
            time_ms: Some(5_000),
            ksteps: 200_000,
            rows: 100_000,
            bytes: 32 << 20,
        }
    }
}

impl Budget {
    /// The tighter of two budgets, limit by limit.
    pub fn min(self, other: Budget) -> Budget {
        Budget {
            time_ms: match (self.time_ms, other.time_ms) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
            ksteps: self.ksteps.min(other.ksteps),
            rows: self.rows.min(other.rows),
            bytes: self.bytes.min(other.bytes),
        }
    }

    /// Parses a `Moul-Budget` header: `time=500, rows=1000, bytes=65536,
    /// ksteps=100`. Unknown keys are ignored.
    pub fn parse_header(value: &str, base: Budget) -> Budget {
        let mut b = base;
        for part in value.split(',') {
            let Some((k, v)) = part.split_once('=') else {
                continue;
            };
            let Ok(n) = v.trim().parse::<u64>() else {
                continue;
            };
            match k.trim() {
                "time" => b.time_ms = Some(n as i64),
                "ksteps" => b.ksteps = n,
                "rows" => b.rows = n,
                "bytes" => b.bytes = n,
                _ => {}
            }
        }
        b.min(base)
    }
}

/// What a request has spent so far.
#[derive(Debug)]
pub struct Meter {
    budget: Budget,
    rows: AtomicU64,
    spent: Arc<Spent>,
}

#[derive(Debug, Default)]
struct Spent {
    ticks: AtomicU64,
    out_of_time: AtomicBool,
    out_of_steps: AtomicBool,
}

impl Meter {
    /// Starts metering under `budget` and installs SQLite's progress
    /// handler on `conn`, reading `clock` for the time limit.
    pub fn start(conn: &mut Connection, budget: Budget, clock: Option<fn() -> i64>) -> Meter {
        let spent = Arc::new(Spent::default());
        let handler_spent = spent.clone();
        let deadline = match (clock, budget.time_ms) {
            (Some(clock), Some(ms)) => Some((clock, clock() + ms)),
            _ => None,
        };
        let max_ticks = budget.ksteps;
        // Called every 1,000 virtual machine steps.
        conn.progress_handler(
            1000,
            Some(move || {
                let ticks = handler_spent.ticks.fetch_add(1, Ordering::Relaxed) + 1;
                if ticks > max_ticks {
                    handler_spent.out_of_steps.store(true, Ordering::Relaxed);
                    return true;
                }
                if let Some((clock, deadline)) = deadline
                    && ticks % 16 == 0
                    && clock() > deadline
                {
                    handler_spent.out_of_time.store(true, Ordering::Relaxed);
                    return true;
                }
                false
            }),
        )
        .ok();
        Meter {
            budget,
            rows: AtomicU64::new(0),
            spent,
        }
    }

    /// Removes the progress handler.
    pub fn stop(conn: &mut Connection) {
        conn.progress_handler(0, None::<fn() -> bool>).ok();
    }

    /// Counts `n` rows read; fails once the row budget is spent.
    pub fn rows(&self, n: u64) -> Result<(), ApiError> {
        let total = self.rows.fetch_add(n, Ordering::Relaxed) + n;
        if total > self.budget.rows {
            return Err(exceeded("rows"));
        }
        Ok(())
    }

    /// Which limit stopped SQLite, if one did.
    pub fn interrupted(&self) -> Option<&'static str> {
        if self.spent.out_of_time.load(Ordering::Relaxed) {
            Some("time")
        } else if self.spent.out_of_steps.load(Ordering::Relaxed) {
            Some("steps")
        } else {
            None
        }
    }

    /// The budget being metered.
    pub fn budget(&self) -> Budget {
        self.budget
    }
}

/// The error for a spent budget.
pub fn exceeded(what: &str) -> ApiError {
    ApiError::bad_request(format!("The request exceeded its {what} budget"))
        .with("budget", json!(what))
}
