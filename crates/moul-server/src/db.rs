//! The engine's own thread, and the queue of work it serves.
//!
//! The engine is synchronous and holds one SQLite connection, so it lives on
//! a thread of its own and everything else sends it work: an HTTP request, a
//! new subscription, a batch of mail to mark as sent. Work runs one item at
//! a time in the order it arrives, which is the order that settles
//! conflicts. After each item the thread hands live events to their streams
//! and wakes whatever waits for commits (the mail, webhook and job
//! dispatchers). Between items it runs its chores when they are due, such as
//! pushing the replica.
//!
//! Moul Hub runs one of these for each open app.

use std::collections::HashMap;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use moul_core::{Engine, Event, Request, Response, Subscription, SubscriptionId};
use tokio::sync::{mpsc, oneshot, watch};

/// A unit of work for the engine's thread.
pub type Task = Box<dyn FnOnce(&mut Db) + Send>;

/// Something the engine's thread does by itself from time to time. Made on
/// the thread, so it need not be `Send`.
pub trait Chore {
    /// A name for logs.
    fn name(&self) -> &str;
    /// How long until it is due again, measured from now.
    fn due_in(&self, db: &Db) -> Duration;
    /// Does the work. Called on the engine's thread, between tasks.
    fn run(&mut self, db: &mut Db);
    /// Called once when the thread stops, so the chore can finish what it
    /// holds (push the last commits).
    fn finish(&mut self, db: &mut Db) {
        self.run(db);
    }
}

/// What a live stream receives.
#[derive(Clone, Debug, PartialEq)]
pub enum StreamMsg {
    /// A record event the subscriber may see.
    Event(Event),
    /// The engine ended the subscription; the reason says why.
    Closed(String),
}

/// The engine's end of one live stream.
struct Sink {
    tx: mpsc::Sender<StreamMsg>,
    slow: Arc<AtomicBool>,
}

/// The receiving end of a live stream, for the HTTP handler.
#[derive(Debug)]
pub struct StreamRx {
    /// The subscription's id in the engine.
    pub id: SubscriptionId,
    /// Events and the closing reason, in order. The channel ends when the
    /// subscription does.
    pub rx: mpsc::Receiver<StreamMsg>,
    /// Set when the stream fell behind and was dropped.
    pub slow: Arc<AtomicBool>,
}

/// How many events a stream may hold before it counts as slow.
pub const STREAM_BUFFER: usize = 256;

/// The engine and what belongs with it on its thread.
pub struct Db {
    /// The engine.
    pub engine: Engine,
    streams: HashMap<SubscriptionId, Sink>,
    nudged: bool,
    started: Instant,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("engine", &self.engine)
            .field("streams", &self.streams.len())
            .finish_non_exhaustive()
    }
}

impl Db {
    /// Wraps an engine.
    pub fn new(engine: Engine) -> Db {
        Db {
            engine,
            streams: HashMap::new(),
            nudged: false,
            started: Instant::now(),
        }
    }

    /// Tells the dispatchers that something may be waiting for them, once
    /// the current task is done.
    pub fn nudge(&mut self) {
        self.nudged = true;
    }

    /// How long the thread has been running.
    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// How many live streams are open.
    pub fn stream_count(&self) -> usize {
        self.streams.len()
    }

    /// Serves one request. Anything but a read nudges the dispatchers.
    pub fn handle(&mut self, req: Request) -> Response {
        let write = req.method != "GET" && req.method != "HEAD";
        let response = self.engine.handle(req);
        if write {
            self.nudge();
        }
        response
    }

    /// Opens a live stream for `sub`.
    pub fn subscribe(&mut self, sub: Subscription) -> Result<StreamRx, Response> {
        let id = self.engine.subscribe(sub)?;
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let slow = Arc::new(AtomicBool::new(false));
        self.streams.insert(
            id,
            Sink {
                tx,
                slow: slow.clone(),
            },
        );
        Ok(StreamRx { id, rx, slow })
    }

    /// Ends every live stream with `reason`, as when the server stops.
    pub fn end_streams(&mut self, reason: &str) {
        for (id, sink) in self.streams.drain().collect::<Vec<_>>() {
            self.engine.unsubscribe(id);
            let _ = sink.tx.try_send(StreamMsg::Closed(reason.to_owned()));
        }
    }

    /// Ends a live stream.
    pub fn unsubscribe(&mut self, id: SubscriptionId) {
        self.engine.unsubscribe(id);
        self.streams.remove(&id);
    }

    /// Hands the engine's deliveries to their streams, and ends streams the
    /// engine ended or whose reader went away or fell behind.
    fn flush(&mut self) {
        for (id, reason) in self.engine.take_closed() {
            if let Some(sink) = self.streams.remove(&id) {
                let _ = sink.tx.try_send(StreamMsg::Closed(reason));
            }
        }
        let mut dead = Vec::new();
        for d in self.engine.take_deliveries() {
            let Some(sink) = self.streams.get(&d.subscription) else {
                continue;
            };
            match sink.tx.try_send(StreamMsg::Event(d.event)) {
                Ok(()) => {}
                Err(mpsc::error::TrySendError::Full(_)) => {
                    sink.slow.store(true, Ordering::SeqCst);
                    dead.push(d.subscription);
                }
                Err(mpsc::error::TrySendError::Closed(_)) => dead.push(d.subscription),
            }
        }
        // A stream whose reader is gone ends here too, even if no event
        // reached it.
        for (&id, sink) in &self.streams {
            if sink.tx.is_closed() {
                dead.push(id);
            }
        }
        for id in dead {
            self.unsubscribe(id);
        }
    }
}

/// The engine's thread stopped, or never started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stopped;

impl std::fmt::Display for Stopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the database is closed")
    }
}

impl std::error::Error for Stopped {}

struct Shared {
    tx: Mutex<Option<std::sync::mpsc::Sender<Task>>>,
    nudges: watch::Sender<u64>,
}

/// Sends work to the engine's thread. Cheap to clone.
#[derive(Clone)]
pub struct DbHandle {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for DbHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DbHandle").finish_non_exhaustive()
    }
}

impl DbHandle {
    /// Runs `f` on the engine's thread and returns what it returns.
    pub async fn run<R: Send + 'static>(
        &self,
        f: impl FnOnce(&mut Db) -> R + Send + 'static,
    ) -> Result<R, Stopped> {
        let (reply, answer) = oneshot::channel();
        self.send(Box::new(move |db: &mut Db| {
            let _ = reply.send(f(db));
        }))?;
        answer.await.map_err(|_| Stopped)
    }

    /// Queues `task` without waiting for it.
    pub fn send(&self, task: Task) -> Result<(), Stopped> {
        let tx = self.shared.tx.lock().unwrap_or_else(|e| e.into_inner());
        tx.as_ref().ok_or(Stopped)?.send(task).map_err(|_| Stopped)
    }

    /// Serves one request on the engine.
    pub async fn handle(&self, req: Request) -> Response {
        match self.run(move |db| db.handle(req)).await {
            Ok(r) => r,
            // The task panicked; the thread rolled it back and goes on.
            Err(Stopped) if self.is_open() => moul_core::ApiError::internal().into_response(),
            Err(Stopped) => moul_core::ApiError::new(503, "The database is closed").into_response(),
        }
    }

    /// Whether the thread still takes work.
    pub fn is_open(&self) -> bool {
        self.shared
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// A receiver that changes after every task that may have queued work
    /// for a dispatcher.
    pub fn nudges(&self) -> watch::Receiver<u64> {
        self.shared.nudges.subscribe()
    }

    /// Stops taking work. Work already queued still runs; the thread then
    /// finishes its chores and closes the engine.
    pub fn close(&self) {
        self.shared
            .tx
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
    }
}

/// The running thread, to wait for when shutting down.
#[derive(Debug)]
pub struct DbThread {
    join: Option<std::thread::JoinHandle<()>>,
}

impl DbThread {
    /// Waits for the thread to finish its queue and chores.
    pub fn join(mut self) {
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// The longest the thread waits without checking its chores.
const MAX_WAIT: Duration = Duration::from_secs(60);

/// What the thread is given to run: the engine and its chores.
pub type Opened = (Db, Vec<Box<dyn Chore>>);

/// Starts the engine's thread. `open` runs on it and builds the [`Db`] and
/// the chores that run between tasks when due (the engine is not `Send`,
/// so it is made where it lives).
pub fn spawn(
    name: &str,
    open: impl FnOnce() -> Result<Opened, String> + Send + 'static,
) -> Result<(DbHandle, DbThread), String> {
    let (tx, rx) = std::sync::mpsc::channel::<Task>();
    let (nudges, _) = watch::channel(0u64);
    let shared = Arc::new(Shared {
        tx: Mutex::new(Some(tx)),
        nudges,
    });
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    let thread_shared = shared.clone();
    let join = std::thread::Builder::new()
        .name(format!("moul-db-{name}"))
        .spawn(move || {
            let (mut db, chores) = match open() {
                Ok(opened) => {
                    let _ = ready_tx.send(Ok(()));
                    opened
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            run_loop(&mut db, rx, chores, &thread_shared);
        })
        .map_err(|e| format!("cannot start the database thread: {e}"))?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok((DbHandle { shared }, DbThread { join: Some(join) })),
        Ok(Err(e)) => {
            let _ = join.join();
            Err(e)
        }
        Err(_) => {
            let _ = join.join();
            Err("the database thread stopped while opening".into())
        }
    }
}

fn run_loop(
    db: &mut Db,
    rx: std::sync::mpsc::Receiver<Task>,
    mut chores: Vec<Box<dyn Chore>>,
    shared: &Shared,
) {
    loop {
        let wait = chores
            .iter()
            .map(|c| c.due_in(db))
            .min()
            .unwrap_or(MAX_WAIT)
            .min(MAX_WAIT);
        match rx.recv_timeout(wait) {
            Ok(task) => {
                run_task(db, task);
                // Drain what else is queued before the chores run.
                while let Ok(task) = rx.try_recv() {
                    run_task(db, task);
                    after_task(db, shared);
                }
                after_task(db, shared);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        for chore in chores.iter_mut() {
            if chore.due_in(db).is_zero() {
                run_chore(db, chore.as_mut(), false);
                after_task(db, shared);
            }
        }
    }
    for chore in chores.iter_mut() {
        run_chore(db, chore.as_mut(), true);
    }
    // Streams end with the engine.
    for (_, sink) in db.streams.drain() {
        let _ = sink
            .tx
            .try_send(StreamMsg::Closed("the server is shutting down".into()));
    }
}

fn after_task(db: &mut Db, shared: &Shared) {
    db.flush();
    if std::mem::take(&mut db.nudged) {
        shared.nudges.send_modify(|n| *n = n.wrapping_add(1));
    }
}

/// Runs a task. A task that panics is logged and its transaction, if it
/// left one open, rolled back, so one bad request cannot stop the database.
fn run_task(db: &mut Db, task: Task) {
    if std::panic::catch_unwind(AssertUnwindSafe(|| task(db))).is_err() {
        recover(db, "a request");
    }
}

fn run_chore(db: &mut Db, chore: &mut dyn Chore, last: bool) {
    let name = chore.name().to_owned();
    let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
        if last {
            chore.finish(db)
        } else {
            chore.run(db)
        }
    }));
    if ran.is_err() {
        recover(db, &name);
    }
}

fn recover(db: &mut Db, what: &str) {
    eprintln!("moul: {what} panicked; its changes were rolled back");
    let conn = db.engine.store().conn();
    if !conn.is_autocommit() {
        let _ = conn.execute_batch("ROLLBACK");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moul_core::{Config, MemoryBlobs, SeededHost};
    use moul_store::Store;
    use serde_json::json;

    fn open() -> Result<Opened, String> {
        let store = Store::open_in_memory().map_err(|e| e.to_string())?;
        let engine = Engine::open(
            store,
            SeededHost::new(1, 0),
            MemoryBlobs::new(),
            Config::default(),
        )
        .map_err(|e| e.to_string())?;
        Ok((Db::new(engine), Vec::new()))
    }

    #[tokio::test]
    async fn serves_requests_in_order_and_survives_a_panic() {
        let (db, thread) = spawn("test", open).unwrap();
        let r = db
            .handle(
                Request::post(
                    "/api/moul",
                    json!({"name": "notes", "type": "base", "fields": [{"name": "title", "type": "text"}]}),
                )
                .owner(),
            )
            .await;
        assert_eq!(r.status, 201, "{:?}", r.json_body());
        let panicked = db
            .run(|db| {
                db.engine
                    .store()
                    .conn()
                    .execute_batch("BEGIN; INSERT INTO notes (id, title, createdAt, updatedAt) VALUES ('x', 'lost', '', '')")
                    .unwrap();
                panic!("boom")
            })
            .await;
        assert_eq!(panicked, Err(Stopped));
        let r = db
            .handle(Request::get("/api/moul/notes/records").owner())
            .await;
        assert_eq!(r.status, 200);
        assert_eq!(
            r.json_body()["totalItems"],
            0,
            "the panicking task's insert was rolled back"
        );
        db.close();
        thread.join();
    }

    #[tokio::test]
    async fn streams_get_events_and_end() {
        let (db, thread) = spawn("test", open).unwrap();
        db.handle(
            Request::post("/api/moul", json!({"name": "notes", "type": "base", "fields": [{"name": "title", "type": "text"}]}))
                .owner(),
        )
        .await;
        let mut stream = db
            .run(|db| {
                db.subscribe(Subscription {
                    collection: "notes".into(),
                    owner: true,
                    ..Subscription::default()
                })
            })
            .await
            .unwrap()
            .unwrap();
        let mut nudges = db.nudges();
        let before = *nudges.borrow_and_update();
        db.handle(Request::post("/api/moul/notes/records", json!({"title": "a"})).owner())
            .await;
        match stream.rx.recv().await {
            Some(StreamMsg::Event(e)) => assert_eq!(e.record["title"], "a"),
            other => panic!("{other:?}"),
        }
        assert_ne!(
            *nudges.borrow_and_update(),
            before,
            "a write nudges the dispatchers"
        );
        db.close();
        thread.join();
        // The stream ends with the thread.
        assert!(matches!(
            stream.rx.recv().await,
            Some(StreamMsg::Closed(_)) | None
        ));
    }
}
