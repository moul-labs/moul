//! Browser smoke tests, run in Chromium, Firefox and WebKit by `browser-tests/run.mjs`.
//!
//! They check the platform features Phase 1 builds on, from inside a dedicated
//! worker, which is where the engine runs.

#![cfg(target_arch = "wasm32")]

use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::{wasm_bindgen_test, wasm_bindgen_test_configure};
use web_sys::{
    FileSystemDirectoryHandle, FileSystemFileHandle, FileSystemGetFileOptions,
    FileSystemReadWriteOptions, FileSystemSyncAccessHandle, WorkerGlobalScope,
};

wasm_bindgen_test_configure!(run_in_dedicated_worker);

const NAME: &str = "moul-smoke-test.bin";

#[wasm_bindgen_test]
fn runs_in_a_dedicated_worker() {
    assert!(js_sys::global().dyn_into::<WorkerGlobalScope>().is_ok());
}

/// Write, flush, read back and truncate an OPFS file through a sync access handle,
/// the call the storage layer makes for every page it reads or writes.
#[wasm_bindgen_test]
async fn opfs_sync_access_handle_round_trip() -> Result<(), JsValue> {
    let scope: WorkerGlobalScope = js_sys::global().unchecked_into();
    let root: FileSystemDirectoryHandle =
        JsFuture::from(scope.navigator().storage().get_directory())
            .await?
            .unchecked_into();

    let create = FileSystemGetFileOptions::new();
    create.set_create(true);
    let file: FileSystemFileHandle =
        JsFuture::from(root.get_file_handle_with_options(NAME, &create))
            .await?
            .unchecked_into();
    let handle: FileSystemSyncAccessHandle = JsFuture::from(file.create_sync_access_handle())
        .await?
        .unchecked_into();

    let page: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    handle.truncate_with_u32(0)?;
    let at = FileSystemReadWriteOptions::new();
    at.set_at(4096.0);
    let written = handle.write_with_u8_array_and_options(&page, &at)?;
    assert_eq!(written as usize, page.len());
    handle.flush()?;
    assert_eq!(
        handle.get_size()? as usize,
        8192,
        "a write past the end grows the file"
    );

    let mut back = vec![0u8; page.len()];
    let read = handle.read_with_u8_array_and_options(&mut back, &at)?;
    assert_eq!(read as usize, page.len());
    assert_eq!(back, page);

    handle.truncate_with_u32(4096)?;
    assert_eq!(handle.get_size()? as usize, 4096);
    handle.close();

    JsFuture::from(root.remove_entry(NAME)).await?;
    Ok(())
}

// ---- Phase 1: storage --------------------------------------------------------

mod storage {
    use std::cell::Cell;
    use std::rc::Rc;

    use js_sys::{Reflect, Uint8Array};
    use moul_store::{FILES, Store};
    use moul_vfs::{OpfsStorage, Volume};
    use moul_web::host::{self, Engine};
    use moul_web::leader::Leadership;
    use moul_web::persist;
    use moul_web::tabs::{LEADER_LOST, Tabs};
    use wasm_bindgen::{JsCast, JsValue};
    use wasm_bindgen_futures::spawn_local;
    use wasm_bindgen_test::wasm_bindgen_test;

    fn unique(tag: &str) -> String {
        format!("moul-test-{tag}-{}", (js_sys::Math::random() * 1e9) as u64)
    }

    fn obj(pairs: &[(&str, JsValue)]) -> JsValue {
        let o = js_sys::Object::new();
        for (k, v) in pairs {
            Reflect::set(&o, &(*k).into(), v).unwrap();
        }
        o.into()
    }

    fn get(v: &JsValue, k: &str) -> JsValue {
        if !v.is_object() {
            return JsValue::UNDEFINED;
        }
        Reflect::get(v, &k.into()).unwrap()
    }

    async fn sleep(ms: i32) {
        let p = js_sys::Promise::new(&mut |resolve, _| {
            let f: js_sys::Function = Reflect::get(&js_sys::global(), &"setTimeout".into())
                .unwrap()
                .unchecked_into();
            f.call2(&JsValue::NULL, &resolve, &ms.into()).unwrap();
        });
        wasm_bindgen_futures::JsFuture::from(p).await.unwrap();
    }

    async fn open_store(path: &str) -> Store {
        let storage = OpfsStorage::open(path, &FILES).await.unwrap();
        Store::open(Volume::open(storage).unwrap()).unwrap()
    }

    /// SQLite on OPFS keeps its data when the files are closed and reopened.
    #[wasm_bindgen_test]
    async fn store_on_opfs_survives_reopening() {
        let path = format!("moul-tests/{}", unique("reopen"));
        {
            let mut store = open_store(&path).await;
            store
                .write(|tx| {
                    tx.execute_batch(
                        "CREATE TABLE notes (body TEXT);
                         CREATE TABLE writer.keys (k TEXT);
                         INSERT INTO keys VALUES ('secret');",
                    )?;
                    for i in 0..500 {
                        tx.execute(
                            "INSERT INTO notes VALUES (?1)",
                            [format!("note {i} {}", "x".repeat(200))],
                        )?;
                    }
                    Ok::<_, rusqlite::Error>(())
                })
                .unwrap();
        }
        let store = open_store(&path).await;
        let n: i64 = store
            .conn()
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 500);
        let ok: String = store
            .conn()
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ok, "ok");
        let ps: i64 = store
            .conn()
            .query_row("PRAGMA page_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ps, 4096,
            "the browser build defaults to 8192; stores pin 4096"
        );

        // Export, then import into a second database.
        let file = store.export().unwrap();
        drop(store);
        let other = format!("moul-tests/{}", unique("import"));
        let mut second = open_store(&other).await;
        second.import(&file).unwrap();
        let n: i64 = second
            .conn()
            .query_row("SELECT count(*) FROM notes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 500);
        let keys: i64 = second
            .conn()
            .query_row("SELECT count(*) FROM writer.sqlite_schema", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(keys, 0, "secrets do not travel with an export");
        drop(second);

        OpfsStorage::remove(&path).await.unwrap();
        OpfsStorage::remove(&other).await.unwrap();
    }

    /// A worker that wants files another holds waits until they are closed.
    #[wasm_bindgen_test]
    async fn opfs_waits_for_the_previous_holder() {
        let path = format!("moul-tests/{}", unique("handoff"));
        let first = OpfsStorage::open(&path, &FILES).await.unwrap();
        let opened = Rc::new(Cell::new(false));
        let flag = opened.clone();
        let p = path.clone();
        spawn_local(async move {
            let second = OpfsStorage::open(&p, &FILES).await;
            assert!(second.is_ok(), "{second:?}");
            flag.set(true);
        });
        sleep(150).await;
        assert!(!opened.get(), "the files are still held");
        drop(first);
        for _ in 0..100 {
            if opened.get() {
                break;
            }
            sleep(20).await;
        }
        assert!(
            opened.get(),
            "the second open went through once the first closed"
        );
        sleep(50).await;
        OpfsStorage::remove(&path).await.unwrap();
    }

    /// One holder of a Web Lock at a time; the next waits and then gets it.
    #[wasm_bindgen_test]
    async fn leader_lock_is_exclusive_and_passes_on() {
        let name = unique("lock");
        let first = Leadership::acquire(&name).await.unwrap();
        assert!(Leadership::try_acquire(&name).await.unwrap().is_none());

        let got = Rc::new(Cell::new(false));
        let flag = got.clone();
        let n = name.clone();
        spawn_local(async move {
            let second = Leadership::acquire(&n).await.unwrap();
            flag.set(true);
            drop(second);
        });
        sleep(50).await;
        assert!(!got.get());
        drop(first);
        for _ in 0..50 {
            if got.get() {
                break;
            }
            sleep(10).await;
        }
        assert!(got.get(), "the waiting context became leader");
        sleep(10).await;
        let third = Leadership::try_acquire(&name).await.unwrap();
        assert!(third.is_some(), "free again once released");
    }

    fn echo_leader() -> moul_web::tabs::Handler {
        let mut count = 0;
        Box::new(move |body, from| {
            count += 1;
            if get(&body, "fail").as_bool() == Some(true) {
                return Err(js_sys::Error::new("refused").into());
            }
            Ok(obj(&[
                ("echo", body),
                ("count", count.into()),
                ("from", from.into()),
            ]))
        })
    }

    fn no_push() -> host::Push {
        Rc::new(|_, _| {})
    }

    /// Requests from a following tab reach the leader and come back.
    #[wasm_bindgen_test]
    async fn requests_reach_the_leader() {
        let channel = unique("tabs");
        let leader = Tabs::join(&channel).unwrap();
        leader.lead(echo_leader());
        let follower = Tabs::join(&channel).unwrap();

        let a = follower.request("hello".into()).await.unwrap();
        assert_eq!(get(&a, "echo").as_string().as_deref(), Some("hello"));
        assert_eq!(follower.leader().as_deref(), Some(leader.tab_id()));

        let err = follower
            .request(obj(&[("fail", true.into())]))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("refused"));

        // The leader's own requests are answered in place.
        let own = leader.request("self".into()).await.unwrap();
        assert_eq!(get(&own, "count").as_f64(), Some(3.0));
    }

    /// A request sent before any tab leads waits for one.
    #[wasm_bindgen_test]
    async fn requests_wait_for_a_leader() {
        let channel = unique("wait");
        let follower = Rc::new(Tabs::join(&channel).unwrap());
        let answer = Rc::new(std::cell::RefCell::new(None));
        let (f, a) = (follower.clone(), answer.clone());
        spawn_local(async move {
            *a.borrow_mut() = Some(f.request("early".into()).await);
        });
        sleep(30).await;
        assert!(answer.borrow().is_none());
        let leader = Tabs::join(&channel).unwrap();
        leader.lead(echo_leader());
        for _ in 0..50 {
            if answer.borrow().is_some() {
                break;
            }
            sleep(10).await;
        }
        let got = answer.borrow_mut().take().expect("answered").unwrap();
        assert_eq!(get(&got, "echo").as_string().as_deref(), Some("early"));
    }

    /// A request in flight when its leader goes away fails, and a new leader
    /// takes the next one.
    #[wasm_bindgen_test]
    async fn a_new_leader_takes_over() {
        let channel = unique("takeover");
        let old = Tabs::join(&channel).unwrap();
        old.lead(echo_leader());
        let follower = Rc::new(Tabs::join(&channel).unwrap());
        follower.request("warm up".into()).await.unwrap();
        assert_eq!(follower.leader().as_deref(), Some(old.tab_id()));

        // The leading tab closes without a word.
        old.disconnect();
        let result = Rc::new(std::cell::RefCell::new(None));
        let (f, r) = (follower.clone(), result.clone());
        spawn_local(async move {
            *r.borrow_mut() = Some(f.request("lost".into()).await);
        });
        sleep(30).await;
        assert!(result.borrow().is_none(), "nobody answers a closed tab");

        // The next tab wins the lock and announces itself.
        let new = Tabs::join(&channel).unwrap();
        new.lead(echo_leader());
        for _ in 0..50 {
            if result.borrow().is_some() {
                break;
            }
            sleep(10).await;
        }
        let lost = result.borrow_mut().take().expect("settled");
        let err = lost.expect_err("the request to the closed tab fails");
        assert!(format!("{err:?}").contains(LEADER_LOST), "{err:?}");
        assert_eq!(follower.leader().as_deref(), Some(new.tab_id()));
        let after = follower.request("after".into()).await.unwrap();
        assert_eq!(get(&after, "echo").as_string().as_deref(), Some("after"));
    }

    /// A tab that resigns hands over cleanly.
    #[wasm_bindgen_test]
    async fn resigning_hands_over() {
        let channel = unique("resign");
        let old = Tabs::join(&channel).unwrap();
        old.lead(echo_leader());
        let follower = Tabs::join(&channel).unwrap();
        follower.request("one".into()).await.unwrap();
        drop(old.resign());
        sleep(20).await;
        assert_eq!(follower.leader(), None);
        let new = Tabs::join(&channel).unwrap();
        new.lead(echo_leader());
        let got = follower.request("two".into()).await.unwrap();
        assert_eq!(
            get(&got, "count").as_f64(),
            Some(1.0),
            "the new leader answered"
        );
    }

    /// The Phase 1 request set, end to end on OPFS.
    #[wasm_bindgen_test]
    async fn engine_answers_sql_export_and_import() {
        let db = unique("engine");
        let opened = host::open(&db).await.unwrap();
        assert!(opened.durable, "{:?}", opened.note);
        let wrote = Rc::new(Cell::new(0));
        let w = wrote.clone();
        let engine = Engine::new(
            opened,
            Some(Box::new(move || w.set(w.get() + 1))),
            no_push(),
        )
        .unwrap();

        let req = |pairs: &[(&str, JsValue)]| obj(pairs);
        engine
            .handle(
                req(&[
                    ("op", "exec".into()),
                    ("sql", "CREATE TABLE t (a, b)".into()),
                ]),
                "me",
            )
            .unwrap();
        let params = js_sys::Array::of3(&1.into(), &"two".into(), &Uint8Array::from(&[3u8, 4][..]));
        engine
            .handle(
                req(&[
                    ("op", "exec".into()),
                    ("sql", "INSERT INTO t VALUES (?1, ?2), (99, ?3)".into()),
                    ("params", params.into()),
                ]),
                "me",
            )
            .unwrap();
        assert_eq!(wrote.get(), 1, "the first-write hook runs once");

        let q = engine
            .handle(
                req(&[
                    ("op", "query".into()),
                    ("sql", "SELECT a, b FROM t ORDER BY a".into()),
                ]),
                "me",
            )
            .unwrap();
        let rows: js_sys::Array = get(&q, "rows").unchecked_into();
        assert_eq!(rows.length(), 2);
        let first: js_sys::Array = rows.get(0).unchecked_into();
        assert_eq!(first.get(0).as_f64(), Some(1.0));
        assert_eq!(first.get(1).as_string().as_deref(), Some("two"));
        let second: js_sys::Array = rows.get(1).unchecked_into();
        assert_eq!(
            second.get(1).dyn_into::<Uint8Array>().unwrap().to_vec(),
            vec![3, 4]
        );

        let exported: Uint8Array = engine
            .handle(req(&[("op", "export".into())]), "me")
            .unwrap()
            .unchecked_into();
        engine
            .handle(
                req(&[("op", "exec".into()), ("sql", "DELETE FROM t".into())]),
                "me",
            )
            .unwrap();
        engine
            .handle(
                req(&[("op", "import".into()), ("bytes", exported.into())]),
                "me",
            )
            .unwrap();
        let q = engine
            .handle(
                req(&[
                    ("op", "query".into()),
                    ("sql", "SELECT count(*) FROM t".into()),
                ]),
                "me",
            )
            .unwrap();
        let rows: js_sys::Array = get(&q, "rows").unchecked_into();
        let row: js_sys::Array = rows.get(0).unchecked_into();
        assert_eq!(row.get(0).as_f64(), Some(2.0));

        let status = engine
            .handle(req(&[("op", "status".into())]), "me")
            .unwrap();
        assert_eq!(get(&status, "durable").as_bool(), Some(true));
        assert!(engine.handle(req(&[("op", "nope".into())]), "me").is_err());
        drop(engine);
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }

    /// Workers can read the persistence state but cannot ask for it.
    #[wasm_bindgen_test]
    async fn persistence_state_is_readable_in_a_worker() {
        let now = persist::persisted().await;
        assert_ne!(
            now,
            persist::Persistence::Unknown,
            "persisted() works in workers"
        );
        assert_eq!(persist::request().await, persist::Persistence::Unknown);
        let (usage, quota) = persist::estimate()
            .await
            .expect("estimate() works in workers");
        assert!(quota > 0.0 && usage >= 0.0);
    }

    /// Two devices on OPFS sync through a replica: the writer's level-0
    /// files come from the pack on OPFS, the follower applies them to its
    /// own OPFS files, and the two end up byte for byte the same.
    #[wasm_bindgen_test]
    async fn two_devices_on_opfs_sync_through_a_replica() {
        use moul_sync::{MemoryReplica, Options, Role, StoreLocal, Sync};
        use moul_vfs::{Capture, PACK_NAME, STATE_NAME};

        async fn device(path: &str) -> (Store, Capture) {
            let storage = OpfsStorage::open(path, &FILES).await.unwrap();
            let ltx = OpfsStorage::open(&format!("{path}/ltx"), &[STATE_NAME, PACK_NAME])
                .await
                .unwrap();
            let capture = Capture::open(ltx, "main.db").unwrap();
            let volume = Volume::open_observed(storage, capture.clone()).unwrap();
            (Store::open_follower(volume).unwrap(), capture)
        }
        async fn run(sync: &mut Sync, store: &mut Store, capture: &Capture) {
            let mut local = StoreLocal {
                store,
                capture: capture.clone(),
                conflicts: None,
            };
            sync.sync(&mut local).await.unwrap();
        }

        let replica = MemoryReplica::new();
        let (pa, pb) = (
            format!("moul-tests/{}", unique("sync-a")),
            format!("moul-tests/{}", unique("sync-b")),
        );
        let (mut a, ca) = device(&pa).await;
        let (mut b, cb) = device(&pb).await;
        let mut sa = Sync::new(replica.clone(), Options::default());
        let mut sb = Sync::new(replica.clone(), Options::default());

        a.conn()
            .execute_batch("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one');")
            .unwrap();
        run(&mut sa, &mut a, &ca).await;
        run(&mut sb, &mut b, &cb).await;
        assert_eq!((sa.role(), sb.role()), (Role::Writer, Role::Follower));
        for i in 0..10 {
            a.conn()
                .execute("INSERT INTO t VALUES (?1)", [format!("row {i}")])
                .unwrap();
        }
        run(&mut sa, &mut a, &ca).await;
        run(&mut sb, &mut b, &cb).await;
        let n: i64 = b
            .conn()
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 11);
        assert!(a.export().unwrap() == b.export().unwrap());

        // Capture keeps the ltx folder's files open; close everything.
        drop((a, b, ca, cb, sa, sb));
        OpfsStorage::remove(&pa).await.unwrap();
        OpfsStorage::remove(&pb).await.unwrap();
    }

    /// Collects what a push function receives, as (tab, message) pairs.
    type Pushed = Rc<std::cell::RefCell<Vec<(Option<String>, JsValue)>>>;

    fn recorder() -> (host::Push, Pushed) {
        let got = Rc::new(std::cell::RefCell::new(Vec::new()));
        let g = got.clone();
        let push: host::Push = Rc::new(move |to: Option<&str>, body| {
            g.borrow_mut().push((to.map(str::to_owned), body));
        });
        (push, got)
    }

    fn api(method: &str, path: &str, json: Option<JsValue>, owner: bool) -> JsValue {
        let mut pairs = vec![
            ("op", JsValue::from("api")),
            ("method", method.into()),
            ("path", path.into()),
            ("owner", owner.into()),
        ];
        if let Some(j) = json {
            pairs.push(("json", j));
        }
        obj(&pairs)
    }

    fn json(text: &str) -> JsValue {
        js_sys::JSON::parse(text).unwrap()
    }

    fn header(response: &JsValue, name: &str) -> Option<String> {
        let headers: js_sys::Array = get(response, "headers").unchecked_into();
        headers.iter().find_map(|pair| {
            let pair: js_sys::Array = pair.unchecked_into();
            (pair.get(0).as_string()?.eq_ignore_ascii_case(name))
                .then(|| pair.get(1).as_string())
                .flatten()
        })
    }

    /// The engine serves HTTP-shaped requests in the worker, announces each
    /// commit to every tab, and routes subscription events to the tab that
    /// subscribed.
    #[wasm_bindgen_test]
    async fn engine_requests_notices_and_subscriptions() {
        let opened = host::open_memory("api-test", None).unwrap();
        let (push, got) = recorder();
        let engine = Engine::new(opened, None, push).unwrap();

        let r = engine
            .handle(
                api(
                    "POST",
                    "/api/moul",
                    Some(json(
                        r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                    )),
                    true,
                ),
                "tab-a",
            )
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        let notes_id = get(&get(&r, "json"), "id").as_string().unwrap();
        {
            let got = got.borrow();
            let (to, notice) = got.last().expect("a notice after the commit");
            assert_eq!(*to, None, "notices go to every tab");
            assert_eq!(get(notice, "type").as_string().as_deref(), Some("changes"));
            let changes: js_sys::Array = get(notice, "changes").unchecked_into();
            assert_eq!(
                get(&changes.get(0), "op").as_string().as_deref(),
                Some("define")
            );
        }

        // Tab b subscribes; an event for it goes to tab b only.
        let sub = engine
            .handle(
                obj(&[
                    ("op", "subscribe".into()),
                    ("collection", "notes".into()),
                    ("owner", true.into()),
                ]),
                "tab-b",
            )
            .unwrap();
        assert_eq!(get(&sub, "ok").as_bool(), Some(true));
        let sub_id = get(&sub, "id");
        got.borrow_mut().clear();
        let r = engine
            .handle(
                api(
                    "POST",
                    "/api/moul/notes/records",
                    Some(json(r#"{"text":"hello"}"#)),
                    true,
                ),
                "tab-a",
            )
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        {
            let got = got.borrow();
            let notice = got
                .iter()
                .find(|(_, m)| get(m, "type").as_string().as_deref() == Some("changes"))
                .expect("a change notice");
            let changes: js_sys::Array = get(&notice.1, "changes").unchecked_into();
            let c = changes.get(0);
            assert_eq!(get(&c, "op").as_string().as_deref(), Some("create"));
            assert_eq!(get(&c, "collection").as_string(), Some(notes_id.clone()));
            let (to, event) = got
                .iter()
                .find(|(_, m)| get(m, "type").as_string().as_deref() == Some("event"))
                .expect("a subscription event");
            assert_eq!(to.as_deref(), Some("tab-b"));
            assert_eq!(get(event, "id").as_f64(), sub_id.as_f64());
            let e = get(event, "event");
            assert_eq!(get(&e, "action").as_string().as_deref(), Some("create"));
            assert_eq!(
                get(&get(&e, "record"), "text").as_string().as_deref(),
                Some("hello")
            );
        }

        // Reads name what they read.
        let list = engine
            .handle(
                api("GET", "/api/moul/notes/records?perPage=5", None, true),
                "tab-a",
            )
            .unwrap();
        assert_eq!(get(&list, "status").as_f64(), Some(200.0));
        assert_eq!(header(&list, "Moul-Reads"), Some(notes_id.clone()));
        let items: js_sys::Array = get(&get(&list, "json"), "items").unchecked_into();
        assert_eq!(items.length(), 1);

        // Only the tab that subscribed can end the subscription, and closed
        // tabs lose theirs.
        let un = |from: &str| {
            engine
                .handle(
                    obj(&[("op", "unsubscribe".into()), ("id", sub_id.clone())]),
                    from,
                )
                .unwrap()
        };
        assert_eq!(get(&un("tab-a"), "ok").as_bool(), Some(false));
        assert_eq!(engine.subscription_count(), 1);
        engine.prune(&std::collections::HashSet::from(["tab-a".to_owned()]));
        assert_eq!(engine.subscription_count(), 0);
        assert_eq!(get(&un("tab-b"), "ok").as_bool(), Some(false));

        // Raw SQL is announced as a change the log does not describe.
        got.borrow_mut().clear();
        engine
            .handle(
                obj(&[
                    ("op", "exec".into()),
                    ("sql", "CREATE TABLE scratch (a)".into()),
                ]),
                "tab-a",
            )
            .unwrap();
        let got = got.borrow();
        assert_eq!(get(&got[0].1, "all").as_bool(), Some(true));
    }

    /// Uploads arrive as form files and come back as bytes.
    #[wasm_bindgen_test]
    async fn engine_takes_form_files_and_answers_bytes() {
        let engine = Engine::new(
            host::open_memory("files-test", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        let file = obj(&[
            ("name", "hello.txt".into()),
            ("type", "text/plain".into()),
            ("data", Uint8Array::from(&b"hello"[..]).into()),
        ]);
        let files = js_sys::Array::of1(&js_sys::Array::of2(&"file".into(), &file));
        let form = obj(&[
            ("fields", js_sys::Array::new().into()),
            ("files", files.into()),
        ]);
        let r = engine
            .handle(
                obj(&[
                    ("op", "api".into()),
                    ("method", "POST".into()),
                    ("path", "/api/upload".into()),
                    ("owner", true.into()),
                    ("form", form),
                ]),
                "me",
            )
            .unwrap();
        assert_eq!(
            get(&r, "status").as_f64(),
            Some(200.0),
            "{:?}",
            get(&r, "json")
        );
        let info: js_sys::Array = get(&r, "json").unchecked_into();
        let url = get(&info.get(0), "url").as_string().expect("a file URL");
        let r = engine
            .handle(obj(&[("op", "api".into()), ("path", url.into())]), "me")
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(200.0));
        assert!(
            get(&r, "type")
                .as_string()
                .unwrap()
                .starts_with("text/plain")
        );
        let bytes: Uint8Array = get(&r, "bytes").unchecked_into();
        assert_eq!(bytes.to_vec(), b"hello".to_vec());
    }

    fn exec(engine: &Engine, sql: &str) -> JsValue {
        engine
            .handle(obj(&[("op", "exec".into()), ("sql", sql.into())]), "me")
            .unwrap()
    }

    fn query(engine: &Engine, sql: &str) -> Result<Vec<Vec<JsValue>>, String> {
        let q = engine
            .handle(obj(&[("op", "query".into()), ("sql", sql.into())]), "me")
            .map_err(|e| get(&e, "message").as_string().unwrap_or_default())?;
        let rows: js_sys::Array = get(&q, "rows").unchecked_into();
        Ok(rows
            .iter()
            .map(|r| r.unchecked_into::<js_sys::Array>().iter().collect())
            .collect())
    }

    fn texts(engine: &Engine) -> Vec<String> {
        query(engine, "SELECT v FROM t ORDER BY rowid")
            .unwrap()
            .into_iter()
            .map(|r| r[0].as_string().unwrap())
            .collect()
    }

    /// Runs a request that answers with a promise.
    async fn settle(engine: &Engine, op: &str) -> Result<JsValue, JsValue> {
        let p = engine.handle(obj(&[("op", op.into())]), "me")?;
        wasm_bindgen_futures::JsFuture::from(p.unchecked_into::<js_sys::Promise>()).await
    }

    /// A device that disconnects keeps what it writes: connecting again
    /// pushes it while the device still writes, and keeps it as a conflict
    /// copy once another device took over.
    #[wasm_bindgen_test]
    async fn an_engine_keeps_what_it_wrote_while_disconnected() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let sync = || {
            Sync::new(
                replica.clone(),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            )
        };
        let open = |name: &str| {
            Engine::new(host::open_memory(name, None).unwrap(), None, no_push()).unwrap()
        };
        let (a, b) = (open("laptop"), open("phone"));
        exec(
            &a,
            "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared')",
        );
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("writer"));
        let s = b.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("follower"));

        // Disconnected, the laptop still writes; connecting again pushes it.
        settle(&a, "syncDisconnect").await.unwrap();
        assert!(!a.syncing());
        exec(&a, "INSERT INTO t VALUES ('disconnected 1')");
        exec(&a, "INSERT INTO t VALUES ('disconnected 2')");
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("writer"));
        assert!(get(&s, "lastError").is_null(), "{s:?}");
        assert_eq!(get(&s, "pending").as_f64(), Some(0.0), "{s:?}");
        settle(&b, "syncNow").await.unwrap();
        assert_eq!(texts(&b), ["shared", "disconnected 1", "disconnected 2"]);

        // Disconnected again while the phone takes over: what the laptop
        // wrote meanwhile is kept as a conflict copy.
        settle(&a, "syncDisconnect").await.unwrap();
        settle(&b, "takeOver").await.unwrap();
        exec(&b, "INSERT INTO t VALUES ('phone')");
        settle(&b, "syncNow").await.unwrap();
        exec(&a, "INSERT INTO t VALUES ('lost?')");
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("follower"));
        assert_eq!(texts(&a), texts(&b));
        let kept: js_sys::Array = settle(&a, "conflicts").await.unwrap().unchecked_into();
        assert_eq!(kept.length(), 1, "{kept:?}");
        let export = a
            .handle(
                obj(&[("op", "conflictExport".into()), ("name", kept.get(0))]),
                "me",
            )
            .unwrap();
        let copy: Uint8Array =
            wasm_bindgen_futures::JsFuture::from(export.unchecked_into::<js_sys::Promise>())
                .await
                .unwrap()
                .unchecked_into();
        let c = Engine::new(host::open_memory("copy", None).unwrap(), None, no_push()).unwrap();
        c.handle(
            obj(&[("op", "import".into()), ("bytes", copy.into())]),
            "me",
        )
        .unwrap();
        assert_eq!(
            texts(&c),
            ["shared", "disconnected 1", "disconnected 2", "lost?"]
        );
    }

    /// `query` runs only statements that read; `exec` runs the rest.
    #[wasm_bindgen_test]
    async fn query_refuses_statements_that_write() {
        let engine = Engine::new(
            host::open_memory("query-test", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        exec(
            &engine,
            "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('one')",
        );
        for sql in [
            "DELETE FROM t RETURNING v",
            "UPDATE t SET v = 'two' RETURNING v",
            "INSERT INTO t VALUES ('three') RETURNING v",
            "CREATE TABLE u (a)",
            "BEGIN",
            "SAVEPOINT a",
            "PRAGMA query_only = 1",
            "PRAGMA foreign_keys = OFF",
            "PRAGMA writable_schema = 1",
            "PRAGMA journal_mode = DELETE",
            "PRAGMA locking_mode = EXCLUSIVE",
            "ATTACH ':memory:' AS x",
            "DETACH device",
        ] {
            let e = query(&engine, sql).unwrap_err();
            assert!(e.contains("use exec"), "{sql}: {e}");
        }
        assert_eq!(texts(&engine), ["one"]);
        // Reading a pragma, or any table, still works.
        let page = query(&engine, "PRAGMA page_size").unwrap();
        assert_eq!(page[0][0].as_f64(), Some(4096.0));
        let fk = query(&engine, "PRAGMA foreign_keys").unwrap();
        assert_eq!(fk.len(), 1);
        let n = query(&engine, "SELECT count(*) FROM device.sqlite_schema").unwrap();
        assert_eq!(n.len(), 1);
        // No setting changed: the engine still writes, and `device` is
        // still attached.
        let qo = query(&engine, "PRAGMA query_only").unwrap();
        assert_eq!(qo[0][0].as_f64(), Some(0.0));
        // No transaction was left open: the engine still writes.
        exec(&engine, "INSERT INTO t VALUES ('four')");
        assert_eq!(texts(&engine), ["one", "four"]);
        let r = engine
            .handle(
                api(
                    "POST",
                    "/api/moul",
                    Some(json(
                        r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                    )),
                    true,
                ),
                "me",
            )
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(201.0));
    }

    /// When the data is replaced in a way the change log does not describe
    /// event by event, every subscription ends, saying why.
    #[wasm_bindgen_test]
    async fn subscriptions_end_when_the_data_is_replaced() {
        let (push, got) = recorder();
        let engine = Engine::new(host::open_memory("replaced", None).unwrap(), None, push).unwrap();
        let r = engine
            .handle(
                api(
                    "POST",
                    "/api/moul",
                    Some(json(
                        r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                    )),
                    true,
                ),
                "tab-a",
            )
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        let subscribe = || {
            let sub = engine
                .handle(
                    obj(&[
                        ("op", "subscribe".into()),
                        ("collection", "notes".into()),
                        ("owner", true.into()),
                    ]),
                    "tab-b",
                )
                .unwrap();
            get(&sub, "id").as_f64().unwrap()
        };
        let closed = |got: &Pushed| -> Vec<(Option<String>, f64, String)> {
            got.borrow()
                .iter()
                .filter(|(_, m)| get(m, "type").as_string().as_deref() == Some("closed"))
                .map(|(to, m)| {
                    (
                        to.clone(),
                        get(m, "id").as_f64().unwrap(),
                        get(m, "reason").as_string().unwrap(),
                    )
                })
                .collect()
        };

        // Raw SQL that leaves the log alone keeps subscriptions.
        let id = subscribe();
        got.borrow_mut().clear();
        exec(&engine, "CREATE TABLE scratch (a)");
        assert!(closed(&got).is_empty());
        assert_eq!(engine.subscription_count(), 1);

        // An import that fails replaces nothing, and ends nothing.
        got.borrow_mut().clear();
        let bad = engine.handle(
            obj(&[
                ("op", "import".into()),
                ("bytes", Uint8Array::from(&b"not a database"[..]).into()),
            ]),
            "tab-a",
        );
        assert!(bad.is_err());
        assert!(got.borrow().is_empty(), "no notice, nothing ended");
        assert_eq!(engine.subscription_count(), 1);

        // An import replaces everything.
        let file = engine
            .handle(obj(&[("op", "export".into())]), "tab-a")
            .unwrap();
        engine
            .handle(
                obj(&[("op", "import".into()), ("bytes", file.clone())]),
                "tab-a",
            )
            .unwrap();
        let ended = closed(&got);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!((ended[0].0.as_deref(), ended[0].1), (Some("tab-b"), id));
        assert!(ended[0].2.contains("import"), "{}", ended[0].2);
        assert_eq!(engine.subscription_count(), 0);

        // A file the engine cannot migrate (a newer format) is refused
        // before the volume takes it: nothing is replaced, nothing ends.
        let newer = {
            let e =
                Engine::new(host::open_memory("newer", None).unwrap(), None, no_push()).unwrap();
            exec(&e, "PRAGMA user_version = 99");
            e.handle(obj(&[("op", "export".into())]), "me").unwrap()
        };
        let id = subscribe();
        got.borrow_mut().clear();
        let failed = engine.handle(obj(&[("op", "import".into()), ("bytes", newer)]), "tab-a");
        assert!(failed.is_err());
        assert!(got.borrow().is_empty(), "no notice, nothing ended");
        assert_eq!(engine.subscription_count(), 1);
        engine
            .handle(obj(&[("op", "import".into()), ("bytes", file)]), "tab-a")
            .unwrap();
        let ended = closed(&got);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].1, id);
        assert_eq!(engine.subscription_count(), 0);

        // A log the engine did not read up to its end, as after a snapshot.
        let id = subscribe();
        got.borrow_mut().clear();
        exec(&engine, "DELETE FROM _changes");
        let ended = closed(&got);
        assert_eq!(ended.len(), 1, "{ended:?}");
        assert_eq!(ended[0].1, id);
        assert!(ended[0].2.contains("snapshot"), "{}", ended[0].2);
        assert_eq!(engine.subscription_count(), 0);
    }

    /// When sync replaces the database underneath the engine (here, joining
    /// a replica while holding data of its own), every subscription ends,
    /// even when the replica's change log runs on past the position the
    /// device had read.
    #[wasm_bindgen_test]
    async fn subscriptions_end_when_sync_replaces_the_database() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let sync = || {
            Sync::new(
                replica.clone(),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            )
        };
        let define = |e: &Engine| {
            let r = e
                .handle(
                    api(
                        "POST",
                        "/api/moul",
                        Some(json(
                            r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                        )),
                        true,
                    ),
                    "tab-a",
                )
                .unwrap();
            assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        };
        let write = |e: &Engine, text: &str| {
            let body = json(&format!(r#"{{"text":"{text}"}}"#));
            let r = e
                .handle(
                    api("POST", "/api/moul/notes/records", Some(body), true),
                    "tab-a",
                )
                .unwrap();
            assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        };

        // The writer's change log runs past the phone's.
        let a = Engine::new(
            host::open_memory("replace-writer", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        define(&a);
        for i in 0..5 {
            write(&a, &format!("writer {i}"));
        }
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("writer"));

        let (push, got) = recorder();
        let b = Engine::new(
            host::open_memory("replace-phone", None).unwrap(),
            None,
            push,
        )
        .unwrap();
        define(&b);
        write(&b, "phone");
        let sub = b
            .handle(
                obj(&[
                    ("op", "subscribe".into()),
                    ("collection", "notes".into()),
                    ("owner", true.into()),
                ]),
                "tab-b",
            )
            .unwrap();
        let id = get(&sub, "id").as_f64().unwrap();
        got.borrow_mut().clear();

        let s = b.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("follower"));
        let kinds = |kind: &str| -> Vec<(Option<String>, JsValue)> {
            got.borrow()
                .iter()
                .filter(|(_, m)| get(m, "type").as_string().as_deref() == Some(kind))
                .cloned()
                .collect()
        };
        let closed: Vec<(Option<String>, f64, String)> = kinds("closed")
            .iter()
            .map(|(to, m)| {
                (
                    to.clone(),
                    get(m, "id").as_f64().unwrap(),
                    get(m, "reason").as_string().unwrap(),
                )
            })
            .collect();
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!((closed[0].0.as_deref(), closed[0].1), (Some("tab-b"), id));
        assert!(closed[0].2.contains("snapshot"), "{}", closed[0].2);
        assert!(
            kinds("event").is_empty(),
            "no event from a log the subscriber did not follow"
        );
        assert_eq!(b.subscription_count(), 0);
        // The phone now holds the writer's records; its own went aside.
        let list = b
            .handle(api("GET", "/api/moul/notes/records", None, true), "tab-a")
            .unwrap();
        assert_eq!(get(&get(&list, "json"), "totalItems").as_f64(), Some(5.0));
        let kept: js_sys::Array = settle(&b, "conflicts").await.unwrap().unchecked_into();
        assert_eq!(kept.length(), 1);
    }

    /// A device whose sync state cannot be read keeps its level-0 files, as
    /// one that joined a replica does: once dropped, they cannot be had
    /// back for the replica.
    #[wasm_bindgen_test]
    async fn an_unreadable_sync_state_keeps_level_0_files() {
        let opened = host::open_memory("bad-state", None).unwrap();
        moul_sync::store_set(&opened.store, "state", Some("not json")).unwrap();
        let capture = opened.capture.clone();
        let engine = Engine::new(opened, None, no_push()).unwrap();
        exec(&engine, "CREATE TABLE t (v TEXT)");
        assert!(capture.first_held().is_some());
    }

    /// A device that disconnects keeps its level-0 files for the next
    /// connect; one that disconnects with `forget` drops them, and every
    /// one after, and joins as a new device would when it connects again:
    /// under a new device id, as a follower, even if it was the writer and
    /// the writer record still names the device it was, and after a reopen.
    #[wasm_bindgen_test]
    async fn disconnecting_with_forget_drops_level_0_files() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let sync = || {
            Sync::new(
                replica.clone(),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            )
        };
        let role = |s: &JsValue| get(s, "role").as_string().unwrap_or_default();
        let db = unique("forget");
        let opened = host::open(&db).await.unwrap();
        let capture = opened.capture.clone();
        let a = Engine::new(opened, None, no_push()).unwrap();
        exec(&a, "CREATE TABLE t (v TEXT)");
        assert_eq!(capture.first_held(), None, "nothing to push: dropped");
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(role(&s), "writer");
        let device = get(&s, "device").as_string().unwrap();
        settle(&a, "syncDisconnect").await.unwrap();
        exec(&a, "INSERT INTO t VALUES ('kept')");
        assert!(capture.first_held().is_some(), "kept for the next connect");

        let p = a
            .handle(
                obj(&[("op", "syncDisconnect".into()), ("forget", true.into())]),
                "me",
            )
            .unwrap();
        let s = wasm_bindgen_futures::JsFuture::from(p.unchecked_into::<js_sys::Promise>())
            .await
            .unwrap();
        assert_eq!(role(&s), "none");
        assert_eq!(capture.first_held(), None, "dropped at once");
        exec(&a, "INSERT INTO t VALUES ('dropped')");
        assert_eq!(capture.first_held(), None, "and after every commit");

        // Connecting again joins the replica anew: what the device holds is
        // kept aside, and it takes the replica's data.
        let s = a.connect_with(sync()).await.unwrap();
        assert!(get(&s, "lastError").is_null(), "{s:?}");
        assert_eq!(role(&s), "follower");
        assert_ne!(get(&s, "device").as_string().unwrap(), device);
        let writer = get(&get(&s, "writer"), "device").as_string();
        assert_eq!(writer.as_deref(), Some(device.as_str()));
        let kept: js_sys::Array = settle(&a, "conflicts").await.unwrap().unchecked_into();
        assert_eq!(kept.length(), 1);
        assert!(texts(&a).is_empty());
        // The status names the copy as the conflict ops take it.
        let listed: js_sys::Array = get(&s, "conflicts").unchecked_into();
        assert_eq!(listed.length(), 1, "{s:?}");
        assert_eq!(listed.get(0).as_string(), kept.get(0).as_string());
        // And it reports the path style in use, which a join link carries.
        assert_eq!(
            get(&get(&s, "settings"), "pathStyle").as_bool(),
            Some(true),
            "{s:?}"
        );

        // After a reopen, as after a reload, it still follows.
        drop((a, capture));
        let a = Engine::new(host::open(&db).await.unwrap(), None, no_push()).unwrap();
        let s = a.connect_with(sync()).await.unwrap();
        assert!(get(&s, "lastError").is_null(), "{s:?}");
        assert_eq!(role(&s), "follower");
        assert_eq!(get(&s, "readOnly").as_bool(), Some(true));
        drop(a);
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
        OpfsStorage::remove(&format!("{}-conflicts", host::opfs_path(&db)))
            .await
            .unwrap();
    }

    /// `syncDisconnect` stops syncing, and forgets the replica if asked,
    /// even when disconnecting the session fails; then it says why.
    #[wasm_bindgen_test]
    async fn a_failed_disconnect_still_stops_and_forgets() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let opened = host::open_memory("disconnect-fails", None).unwrap();
        let capture = opened.capture.clone();
        let a = Engine::new(opened, None, no_push()).unwrap();
        exec(&a, "CREATE TABLE t (v TEXT)");
        let sync = Sync::new(
            MemoryReplica::new(),
            Options {
                poll_every: 0,
                ..Options::default()
            },
        );
        a.connect_with(sync).await.unwrap();
        exec(&a, "INSERT INTO t VALUES ('held')");
        assert!(capture.first_held().is_some());
        // The engine is closing: the session cannot run the disconnect.
        a.shut_down();
        let p = a
            .handle(
                obj(&[("op", "syncDisconnect".into()), ("forget", true.into())]),
                "me",
            )
            .unwrap();
        let e = wasm_bindgen_futures::JsFuture::from(p.unchecked_into::<js_sys::Promise>())
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains("closing"), "{e:?}");
        assert!(!a.syncing(), "the settings went");
        let s = a.handle(obj(&[("op", "syncStatus".into())]), "me").unwrap();
        assert_eq!(get(&s, "configured").as_bool(), Some(false));
        assert_eq!(capture.first_held(), None, "the replica was forgotten");
    }

    /// A replica that answers after 100 ms.
    struct Slow(moul_sync::MemoryReplica);

    impl moul_sync::Replica for Slow {
        fn describe(&self) -> String {
            "slow".into()
        }
        fn get<'a>(
            &'a self,
            key: &'a str,
        ) -> moul_sync::BoxFuture<'a, moul_sync::Result<Option<moul_sync::Object>>> {
            Box::pin(async move {
                sleep(100).await;
                self.0.get(key).await
            })
        }
        fn put<'a>(
            &'a self,
            key: &'a str,
            bytes: &'a [u8],
            condition: moul_sync::Condition,
        ) -> moul_sync::BoxFuture<'a, moul_sync::Result<moul_sync::Put>> {
            Box::pin(async move {
                sleep(100).await;
                self.0.put(key, bytes, condition).await
            })
        }
        fn list<'a>(
            &'a self,
            prefix: &'a str,
        ) -> moul_sync::BoxFuture<'a, moul_sync::Result<Vec<moul_sync::ObjectInfo>>> {
            Box::pin(async move {
                sleep(100).await;
                self.0.list(prefix).await
            })
        }
        fn delete<'a>(&'a self, key: &'a str) -> moul_sync::BoxFuture<'a, moul_sync::Result<()>> {
            Box::pin(async move {
                sleep(100).await;
                self.0.delete(key).await
            })
        }
    }

    /// A sync round holds the engine until it ends; once the engine is shut
    /// down, no new round starts.
    #[wasm_bindgen_test]
    async fn a_sync_round_holds_the_engine_until_it_ends() {
        use moul_sync::MemoryReplica;

        let engine =
            Engine::new(host::open_memory("slow", None).unwrap(), None, no_push()).unwrap();
        let weak = engine.downgrade();
        let done = Rc::new(Cell::new(false));
        let (e, d) = (engine.clone(), done.clone());
        spawn_local(async move {
            let sync = moul_sync::Sync::new(Slow(MemoryReplica::new()), Default::default());
            let _ = e.connect_with(sync).await;
            d.set(true);
        });
        sleep(30).await;
        engine.shut_down();
        drop(engine);
        assert!(weak.is_open(), "the round still holds the engine");
        for _ in 0..200 {
            if !weak.is_open() {
                break;
            }
            sleep(20).await;
        }
        assert!(done.get() && !weak.is_open(), "let go once the round ended");

        // A shut-down engine starts no round.
        let engine =
            Engine::new(host::open_memory("slow-2", None).unwrap(), None, no_push()).unwrap();
        engine.shut_down();
        let sync = moul_sync::Sync::new(Slow(MemoryReplica::new()), Default::default());
        let err = engine.connect_with(sync).await.unwrap_err();
        assert!(format!("{err:?}").contains("closing"), "{err:?}");
    }

    /// `holds_data` tells whether OPFS keeps a database by that name.
    #[wasm_bindgen_test]
    async fn holds_data_finds_a_database_in_opfs() {
        let db = unique("holds");
        assert!(!host::holds_data(&db).await);
        let engine = Engine::new(host::open(&db).await.unwrap(), None, no_push()).unwrap();
        exec(&engine, "CREATE TABLE t (v TEXT)");
        drop(engine);
        assert!(host::holds_data(&db).await);
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
        assert!(!host::holds_data(&db).await);
    }

    /// A commit lands in the commit log before it reaches the shared store:
    /// a log with a commit in it is data, even with no shared store.
    #[wasm_bindgen_test]
    async fn holds_data_counts_a_commit_in_the_log() {
        use moul_vfs::{LOG_HEADER_LEN, LOG_NAME, Storage};
        let db = unique("holds-log");
        let path = host::opfs_path(&db);
        let write_log = |len: u64| {
            let path = path.clone();
            async move {
                let storage = OpfsStorage::open(&path, &[]).await.unwrap();
                let mut log = storage.open(LOG_NAME).unwrap();
                log.truncate(0).unwrap();
                log.write_at(&vec![1; len as usize], 0).unwrap();
                log.flush().unwrap();
            }
        };
        write_log(LOG_HEADER_LEN).await;
        assert!(
            !host::holds_data(&db).await,
            "a log with only its header holds no commit"
        );
        write_log(LOG_HEADER_LEN + 64).await;
        assert!(host::holds_data(&db).await, "a commit in the log is data");
        OpfsStorage::remove(&path).await.unwrap();
    }

    /// Calls `method` on `target` and waits for the promise it returns.
    async fn call(target: &JsValue, method: &str, args: &[JsValue]) -> Result<JsValue, JsValue> {
        let f: js_sys::Function = Reflect::get(target, &method.into())?.dyn_into()?;
        let p = f.apply(target, &args.iter().collect::<js_sys::Array>())?;
        wasm_bindgen_futures::JsFuture::from(p.dyn_into::<js_sys::Promise>()?).await
    }

    /// The OPFS folder at `path`, made if missing.
    async fn opfs_folder(path: &str) -> JsValue {
        let navigator = Reflect::get(&js_sys::global(), &"navigator".into()).unwrap();
        let storage = Reflect::get(&navigator, &"storage".into()).unwrap();
        let mut dir = call(&storage, "getDirectory", &[]).await.unwrap();
        for part in path.split('/') {
            dir = call(
                &dir,
                "getDirectoryHandle",
                &[part.into(), obj(&[("create", true.into())])],
            )
            .await
            .unwrap();
        }
        dir
    }

    /// A leader that cannot open the database answers every request with
    /// the reason, keeps trying, and serves the database once it opens,
    /// without a reload. Closed meanwhile, it lets go of the lock at once.
    #[wasm_bindgen_test]
    async fn a_leader_that_could_not_open_recovers() {
        use moul_web::leader::{Leadership, lock_name};
        use moul_web::{MoulWorker, Retry};
        let retry = Retry {
            attempts: 2,
            delay_ms: 20,
            max_delay_ms: 100,
        };
        let status = || obj(&[("op", "status".into())]);
        // A folder where the shared store goes: opening fails at once.
        let block = |db: String| async move {
            let folder = opfs_folder(&host::opfs_path(&db)).await;
            call(
                &folder,
                "getDirectoryHandle",
                &["main.db".into(), obj(&[("create", true.into())])],
            )
            .await
            .unwrap();
            folder
        };

        let db = unique("recover");
        let folder = block(db.clone()).await;
        let worker = MoulWorker::with_retry(&db, retry).unwrap();
        let pushes = js_sys::Array::new();
        let keep = pushes.clone();
        let listener =
            wasm_bindgen::closure::Closure::<dyn FnMut(JsValue)>::new(move |p: JsValue| {
                keep.push(&p);
            });
        worker.on_push(Some(
            listener
                .as_ref()
                .unchecked_ref::<js_sys::Function>()
                .clone(),
        ));
        // Another tab of the same database, which never leads.
        let other = Tabs::join(&lock_name(&db)).unwrap();
        let heard = js_sys::Array::new();
        let keep = heard.clone();
        let other_listener =
            wasm_bindgen::closure::Closure::<dyn FnMut(JsValue)>::new(move |p: JsValue| {
                keep.push(&p);
            });
        other.on_push(Some(
            other_listener
                .as_ref()
                .unchecked_ref::<js_sys::Function>()
                .clone(),
        ));
        let e = worker.request(status()).await.unwrap_err();
        let message = get(&e, "message").as_string().unwrap_or_default();
        assert!(message.contains("could not be opened"), "{message}");
        assert!(worker.is_leader());
        call(
            &folder,
            "removeEntry",
            &["main.db".into(), obj(&[("recursive", true.into())])],
        )
        .await
        .unwrap();
        let mut answer = Err(JsValue::NULL);
        for _ in 0..100 {
            answer = worker.request(status()).await;
            if answer.is_ok() {
                break;
            }
            sleep(20).await;
        }
        let answer = answer.expect("the leader opened the database");
        assert_eq!(get(&answer, "durable").as_bool(), Some(true));
        assert_eq!(worker.last_error(), None);
        // The leader did not change, so pages hear that the database opened.
        sleep(0).await;
        let opened = pushes.iter().any(|p| {
            get(&p, "type").as_string().as_deref() == Some("leader")
                && get(&p, "opened").as_bool() == Some(true)
        });
        assert!(opened, "{pushes:?}");
        // The other tab hears it too, through the channel.
        let is_opened = |p: &JsValue| {
            get(p, "type").as_string().as_deref() == Some("leader")
                && get(p, "opened").as_bool() == Some(true)
        };
        for _ in 0..50 {
            if heard.iter().any(|p| is_opened(&p)) {
                break;
            }
            sleep(20).await;
        }
        assert!(heard.iter().any(|p| is_opened(&p)), "{heard:?}");
        other.on_push(None);
        drop((other, other_listener));
        worker.on_push(None);
        drop(listener);
        worker.close();
        sleep(100).await;
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();

        // Closed while it answers with the error: the next tab may lead.
        let db = unique("recover-close");
        block(db.clone()).await;
        let worker = MoulWorker::with_retry(&db, retry).unwrap();
        worker.request(status()).await.unwrap_err();
        worker.close();
        let mut free = None;
        for _ in 0..50 {
            free = Leadership::try_acquire(&lock_name(&db)).await.unwrap();
            if free.is_some() {
                break;
            }
            sleep(20).await;
        }
        assert!(free.is_some(), "the lock was let go");
        drop(free);
        sleep(100).await;
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }

    /// A listener runs outside the leader's answer, so it may call back
    /// into its tab: ask whether it leads, send a request, resign.
    #[wasm_bindgen_test]
    async fn a_listener_may_call_back_into_its_tab() {
        let channel = unique("reenter");
        let leader = Rc::new(Tabs::join(&channel).unwrap());
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let (l, lg) = (Rc::downgrade(&leader), log.clone());
        let listener =
            wasm_bindgen::closure::Closure::<dyn FnMut(JsValue)>::new(move |m: JsValue| {
                let Some(l) = l.upgrade() else {
                    return;
                };
                let kind = get(&m, "type").as_string().unwrap_or_default();
                lg.borrow_mut().push(format!("{kind}:{}", l.is_leader()));
                match kind.as_str() {
                    "one" => spawn_local(async move {
                        l.request("two".into()).await.unwrap();
                    }),
                    "bye" => drop(l.resign()),
                    _ => {}
                }
            });
        leader.on_push(Some(
            listener
                .as_ref()
                .unchecked_ref::<js_sys::Function>()
                .clone(),
        ));
        let me = Rc::downgrade(&leader);
        leader.lead(Box::new(move |body, _| {
            if let Some(l) = me.upgrade() {
                l.push(None, obj(&[("type", body.clone())]));
                l.push(Some(l.tab_id()), obj(&[("type", "to-me".into())]));
            }
            Ok(JsValue::NULL)
        }));
        let other = Tabs::join(&channel).unwrap();
        sleep(20).await;
        assert_eq!(other.leader().as_deref(), Some(leader.tab_id()));
        leader.request("one".into()).await.unwrap();
        sleep(20).await;
        leader.request("bye".into()).await.unwrap();
        sleep(20).await;
        assert_eq!(
            *log.borrow(),
            [
                "leader:true",
                "one:true",
                "to-me:true",
                "two:true",
                "to-me:true",
                "bye:true",
                "to-me:false",
            ]
        );
        assert!(!leader.is_leader());
        assert_eq!(other.leader(), None, "the other tab heard it resign");
        drop(listener);
    }

    /// A worker that closes, here from its own listener, hands the files to
    /// the next tab's worker, which opens them and answers.
    #[wasm_bindgen_test]
    async fn a_closed_worker_hands_its_files_to_the_next() {
        use moul_web::MoulWorker;
        let db = unique("handover");
        let first = Rc::new(MoulWorker::new(&db).unwrap());
        let seen = Rc::new(std::cell::RefCell::new(Vec::new()));
        let (w, s) = (Rc::downgrade(&first), seen.clone());
        let listener =
            wasm_bindgen::closure::Closure::<dyn FnMut(JsValue)>::new(move |m: JsValue| {
                if let Some(w) = w.upgrade()
                    && get(&m, "type").as_string().as_deref() == Some("changes")
                {
                    s.borrow_mut().push(w.is_leader());
                    w.close();
                }
            });
        first.on_push(Some(
            listener
                .as_ref()
                .unchecked_ref::<js_sys::Function>()
                .clone(),
        ));
        first
            .request(obj(&[
                ("op", "exec".into()),
                (
                    "sql",
                    "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('kept')".into(),
                ),
            ]))
            .await
            .unwrap();
        for _ in 0..50 {
            if !seen.borrow().is_empty() {
                break;
            }
            sleep(10).await;
        }
        assert_eq!(*seen.borrow(), [true]);
        assert!(!first.is_leader());

        let second = MoulWorker::new(&db).unwrap();
        let q = second
            .request(obj(&[
                ("op", "query".into()),
                ("sql", "SELECT v FROM t".into()),
            ]))
            .await
            .unwrap();
        let rows: js_sys::Array = get(&q, "rows").unchecked_into();
        assert_eq!(rows.length(), 1);
        assert!(second.is_leader());
        second.close();
        drop(listener);
        sleep(100).await;
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }

    /// The leader pushes to one tab or to all, and every tab hears when the
    /// leader changes.
    #[wasm_bindgen_test]
    async fn pushes_reach_their_tabs() {
        let channel = unique("push");
        let leader = Rc::new(Tabs::join(&channel).unwrap());
        let a = Tabs::join(&channel).unwrap();
        let b = Tabs::join(&channel).unwrap();
        let heard = |tag: &'static str, log: Rc<std::cell::RefCell<Vec<String>>>| {
            wasm_bindgen::closure::Closure::<dyn FnMut(JsValue)>::new(move |m: JsValue| {
                let kind = get(&m, "type").as_string().unwrap_or_default();
                let text = get(&m, "text").as_string().unwrap_or_default();
                log.borrow_mut().push(format!("{tag}:{kind}:{text}"));
            })
        };
        let log = Rc::new(std::cell::RefCell::new(Vec::new()));
        let ha = heard("a", log.clone());
        let hb = heard("b", log.clone());
        let hl = heard("l", log.clone());
        a.on_push(Some(
            ha.as_ref().unchecked_ref::<js_sys::Function>().clone(),
        ));
        b.on_push(Some(
            hb.as_ref().unchecked_ref::<js_sys::Function>().clone(),
        ));
        leader.on_push(Some(
            hl.as_ref().unchecked_ref::<js_sys::Function>().clone(),
        ));

        let l = Rc::downgrade(&leader);
        leader.lead(Box::new(move |body, from| {
            if let Some(l) = l.upgrade() {
                l.push(
                    Some(from),
                    obj(&[("type", "to".into()), ("text", body.clone())]),
                );
                l.push(None, obj(&[("type", "all".into()), ("text", body)]));
            }
            Ok(JsValue::NULL)
        }));
        sleep(50).await;
        a.request("one".into()).await.unwrap();
        sleep(50).await;
        let mut seen = log.borrow().clone();
        seen.sort();
        assert!(seen.contains(&"a:leader:".to_owned()), "{seen:?}");
        assert!(seen.contains(&"b:leader:".to_owned()), "{seen:?}");
        assert!(seen.contains(&"a:to:one".to_owned()), "{seen:?}");
        assert!(!seen.iter().any(|s| s.starts_with("b:to")), "{seen:?}");
        for tab in ["a", "b", "l"] {
            assert!(seen.contains(&format!("{tab}:all:one")), "{seen:?}");
        }

        // Tabs that hold their liveness lock are live.
        sleep(50).await;
        let live = moul_web::tabs::live_tabs(&channel).await.unwrap();
        assert!(
            live.contains(a.tab_id()) && live.contains(b.tab_id()),
            "{live:?}"
        );
        drop((a, b, ha, hb, hl));
    }

    /// While sync joins a replica (it keeps a copy, then downloads the
    /// replica's state over the database), the device takes no writes:
    /// every write it acknowledged is in the database or in the copy. And
    /// the first request after the database was replaced ends every
    /// subscription, before the sync call itself ends.
    #[wasm_bindgen_test]
    async fn requests_while_sync_replaces_the_database() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let notes = || {
            api(
                "POST",
                "/api/moul",
                Some(json(
                    r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                )),
                true,
            )
        };
        let write = |e: &Engine, text: &str| -> f64 {
            let body = json(&format!(r#"{{"text":"{text}"}}"#));
            let r = e
                .handle(
                    api("POST", "/api/moul/notes/records", Some(body), true),
                    "tab-a",
                )
                .unwrap();
            get(&r, "status").as_f64().unwrap()
        };
        let records = |e: &Engine| -> Vec<String> {
            let r = e
                .handle(
                    api("GET", "/api/moul/notes/records?perPage=500", None, true),
                    "tab-a",
                )
                .unwrap();
            let items: js_sys::Array = get(&get(&r, "json"), "items").unchecked_into();
            items
                .iter()
                .map(|i| get(&i, "text").as_string().unwrap())
                .collect()
        };

        // The writer pushes a snapshot and three files after it.
        let a = Engine::new(
            host::open_memory("swap-writer", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        a.handle(notes(), "tab-a").unwrap();
        let s = a
            .connect_with(Sync::new(
                replica.clone(),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            ))
            .await
            .unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("writer"));
        for i in 0..3 {
            assert_eq!(write(&a, &format!("writer {i}")), 201.0);
        }
        settle(&a, "syncNow").await.unwrap();

        // The phone, with data and a subscription of its own, joins through
        // a slow connection; a tab keeps writing meanwhile.
        let (push, got) = recorder();
        let b = Engine::new(host::open_memory("swap-phone", None).unwrap(), None, push).unwrap();
        b.handle(notes(), "tab-a").unwrap();
        assert_eq!(write(&b, "phone"), 201.0);
        let sub = b
            .handle(
                obj(&[
                    ("op", "subscribe".into()),
                    ("collection", "notes".into()),
                    ("owner", true.into()),
                ]),
                "tab-b",
            )
            .unwrap();
        assert_eq!(get(&sub, "ok").as_bool(), Some(true));
        let done = Rc::new(Cell::new(false));
        let (e, d) = (b.clone(), done.clone());
        spawn_local(async move {
            e.connect_with(Sync::new(
                Slow(replica.clone()),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            ))
            .await
            .unwrap();
            d.set(true);
        });
        let closed = || {
            got.borrow()
                .iter()
                .any(|(_, m)| get(m, "type").as_string().as_deref() == Some("closed"))
        };
        let (mut acked, mut refused, mut ended_mid_call) = (Vec::new(), 0, false);
        for i in 0..200 {
            if done.get() {
                break;
            }
            let text = format!("during {i}");
            if write(&b, &text) == 201.0 {
                acked.push(text);
            } else {
                refused += 1;
            }
            ended_mid_call |= closed() && !done.get();
            sleep(20).await;
        }
        assert!(done.get(), "joined");
        assert!(refused > 0, "no write came while joining");
        assert!(ended_mid_call, "the subscription ended at the next request");
        assert_eq!(b.subscription_count(), 0);

        // Every acknowledged write is in the database or in the copy.
        let mut seen = records(&b);
        let kept: js_sys::Array = settle(&b, "conflicts").await.unwrap().unchecked_into();
        assert_eq!(kept.length(), 1, "{kept:?}");
        let export = b
            .handle(
                obj(&[("op", "conflictExport".into()), ("name", kept.get(0))]),
                "me",
            )
            .unwrap();
        let copy: Uint8Array =
            wasm_bindgen_futures::JsFuture::from(export.unchecked_into::<js_sys::Promise>())
                .await
                .unwrap()
                .unchecked_into();
        let c = Engine::new(
            host::open_memory("swap-copy", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        c.handle(
            obj(&[("op", "import".into()), ("bytes", copy.into())]),
            "me",
        )
        .unwrap();
        seen.extend(records(&c));
        assert!(seen.contains(&"phone".to_owned()), "{seen:?}");
        for text in &acked {
            assert!(seen.contains(text), "{text:?} was acknowledged, then lost");
        }
        let mut writer = records(&a);
        let mut phone = records(&b);
        writer.sort();
        phone.sort();
        assert_eq!(phone, writer);
    }

    /// When the browser refuses OPFS itself (`getDirectory()` fails with
    /// `UnknownError`, as WebKit's private windows answer), nothing stored
    /// there can be read or overwritten: the engine runs in memory and says
    /// so, whatever else the origin stores (here a cache, as a service
    /// worker keeps one).
    #[wasm_bindgen_test]
    async fn a_refused_file_system_runs_in_memory() {
        use moul_web::{MoulWorker, Retry};
        let run = |body: &str| {
            let v = js_sys::Function::new_no_args(body)
                .call0(&JsValue::NULL)
                .unwrap();
            async move {
                if let Ok(p) = v.dyn_into::<js_sys::Promise>() {
                    wasm_bindgen_futures::JsFuture::from(p).await.unwrap();
                }
            }
        };
        run(r#"
            const proto = Object.getPrototypeOf(navigator.storage);
            self.moulGetDirectory = proto.getDirectory;
            proto.getDirectory = function () {
                return Promise.reject(new DOMException("refused for the test", "UnknownError"));
            };
            if (typeof caches === "undefined") return null;
            return caches.open("moul-test-private")
                .then((c) => c.put("/moul-test", new Response("cached")));
        "#)
        .await;
        let db = unique("private");
        let worker = MoulWorker::with_retry(
            &db,
            Retry {
                attempts: 10,
                delay_ms: 20,
                max_delay_ms: 100,
            },
        )
        .unwrap();
        let status = worker.request(obj(&[("op", "status".into())])).await;
        run(r#"
            Object.getPrototypeOf(navigator.storage).getDirectory = self.moulGetDirectory;
            delete self.moulGetDirectory;
            if (typeof caches === "undefined") return null;
            return caches.delete("moul-test-private");
        "#)
        .await;
        let status = status.expect("the engine runs in memory");
        assert_eq!(get(&status, "durable").as_bool(), Some(false));
        let note = get(&status, "note").as_string().unwrap_or_default();
        assert!(
            note.contains("file system") && note.contains("memory"),
            "{note}"
        );
        worker.close();
    }

    /// A worker dropped without `close()` while it answers with an error
    /// stops trying, and lets go of the leader lock.
    #[wasm_bindgen_test]
    async fn a_dropped_worker_lets_go_of_the_lock() {
        use moul_web::leader::{Leadership, lock_name};
        use moul_web::{MoulWorker, Retry};
        let db = unique("dropped");
        // A folder where the shared store goes: opening fails at once.
        let folder = opfs_folder(&host::opfs_path(&db)).await;
        call(
            &folder,
            "getDirectoryHandle",
            &["main.db".into(), obj(&[("create", true.into())])],
        )
        .await
        .unwrap();
        let worker = MoulWorker::with_retry(
            &db,
            Retry {
                attempts: 2,
                delay_ms: 20,
                max_delay_ms: 100,
            },
        )
        .unwrap();
        let e = worker
            .request(obj(&[("op", "status".into())]))
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains("could not be opened"), "{e:?}");
        assert!(
            Leadership::try_acquire(&lock_name(&db))
                .await
                .unwrap()
                .is_none()
        );
        drop(worker);
        let mut free = None;
        for _ in 0..50 {
            free = Leadership::try_acquire(&lock_name(&db)).await.unwrap();
            if free.is_some() {
                break;
            }
            sleep(20).await;
        }
        assert!(free.is_some(), "the lock was let go");
        drop(free);
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }

    /// The first request after sync replaced the database, while the sync
    /// call still runs, ends the subscriptions that followed the old data
    /// before it is answered: a subscription it makes starts on the new
    /// data and lives on.
    #[wasm_bindgen_test]
    async fn a_subscription_made_after_a_replace_lives_on() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let options = || Options {
            poll_every: 0,
            ..Options::default()
        };
        let notes = || {
            api(
                "POST",
                "/api/moul",
                Some(json(
                    r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                )),
                true,
            )
        };

        // The writer pushes a snapshot and files after it, so the phone
        // still downloads once its database is replaced.
        let a = Engine::new(
            host::open_memory("resub-writer", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        a.handle(notes(), "tab-a").unwrap();
        a.connect_with(Sync::new(replica.clone(), options()))
            .await
            .unwrap();
        for i in 0..3 {
            let body = json(&format!(r#"{{"text":"writer {i}"}}"#));
            let r = a
                .handle(
                    api("POST", "/api/moul/notes/records", Some(body), true),
                    "tab-a",
                )
                .unwrap();
            assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        }
        settle(&a, "syncNow").await.unwrap();

        let (push, got) = recorder();
        let b = Engine::new(host::open_memory("resub-phone", None).unwrap(), None, push).unwrap();
        b.handle(notes(), "tab-a").unwrap();
        let subscribe = |b: &Engine| -> Option<f64> {
            let sub = b
                .handle(
                    obj(&[
                        ("op", "subscribe".into()),
                        ("collection", "notes".into()),
                        ("owner", true.into()),
                    ]),
                    "tab-b",
                )
                .unwrap();
            (get(&sub, "ok").as_bool() == Some(true)).then(|| get(&sub, "id").as_f64().unwrap())
        };
        let closed_since = |got: &Pushed, from: usize| -> Vec<f64> {
            got.borrow()[from..]
                .iter()
                .filter(|(_, m)| get(m, "type").as_string().as_deref() == Some("closed"))
                .map(|(_, m)| get(m, "id").as_f64().unwrap())
                .collect()
        };
        let first = subscribe(&b).unwrap();
        let done = Rc::new(Cell::new(false));
        let (e, d) = (b.clone(), done.clone());
        spawn_local(async move {
            e.connect_with(Sync::new(Slow(replica.clone()), options()))
                .await
                .unwrap();
            d.set(true);
        });
        let mut after_replace = Vec::new();
        for _ in 0..200 {
            if done.get() {
                break;
            }
            let seen = got.borrow().len();
            let replaced_before = !closed_since(&got, 0).is_empty();
            if let Some(id) = subscribe(&b) {
                let ended = closed_since(&got, seen);
                assert!(
                    !ended.contains(&id),
                    "subscription {id} ended by the request that made it"
                );
                if (replaced_before || !ended.is_empty()) && !done.get() {
                    after_replace.push(id);
                }
            }
            sleep(20).await;
        }
        assert!(done.get(), "joined");
        let closed = closed_since(&got, 0);
        assert!(closed.contains(&first), "the old subscription ended");
        assert!(
            !after_replace.is_empty(),
            "no subscription was made while the call ran on"
        );
        for id in &after_replace {
            assert!(!closed.contains(id), "subscription {id} ended: {closed:?}");
        }
        assert_eq!(b.subscription_count(), after_replace.len());
    }

    /// `syncDisconnect` leaves the device with no role even when the
    /// session could not disconnect: a follower must not reopen read-only
    /// for good, with no sync left to settle its role.
    #[wasm_bindgen_test]
    async fn a_follower_whose_disconnect_failed_reopens_writable() {
        use moul_sync::{MemoryReplica, Options, Sync};
        let replica = MemoryReplica::new();
        let sync = || {
            Sync::new(
                replica.clone(),
                Options {
                    poll_every: 0,
                    ..Options::default()
                },
            )
        };
        let w = Engine::new(
            host::open_memory("role-writer", None).unwrap(),
            None,
            no_push(),
        )
        .unwrap();
        exec(
            &w,
            "CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('shared')",
        );
        w.connect_with(sync()).await.unwrap();
        let db = unique("disconnect-role");
        let a = Engine::new(host::open(&db).await.unwrap(), None, no_push()).unwrap();
        let s = a.connect_with(sync()).await.unwrap();
        assert_eq!(get(&s, "role").as_string().as_deref(), Some("follower"));

        // The engine is closing: the session cannot run the disconnect.
        a.shut_down();
        let e = settle(&a, "syncDisconnect").await.unwrap_err();
        assert!(format!("{e:?}").contains("closing"), "{e:?}");
        drop(a);

        // After a reopen, as after a reload, the device writes.
        let a = Engine::new(host::open(&db).await.unwrap(), None, no_push()).unwrap();
        assert!(!a.syncing());
        let s = a.handle(obj(&[("op", "status".into())]), "me").unwrap();
        assert_eq!(get(&s, "readOnly").as_bool(), Some(false), "{s:?}");
        exec(&a, "INSERT INTO t VALUES ('mine')");
        assert_eq!(texts(&a), ["shared", "mine"]);
        drop(a);
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }

    /// A subscribe answer names the leading tab that made the subscription,
    /// whether the leader asked or another tab's request was forwarded.
    #[wasm_bindgen_test]
    async fn subscribe_answers_name_the_leading_tab() {
        use moul_web::MoulWorker;
        let db = unique("sub-leader");
        let leader = MoulWorker::new(&db).unwrap();
        let r = leader
            .request(api(
                "POST",
                "/api/moul",
                Some(json(
                    r#"{"name":"notes","fields":[{"name":"text","type":"text"}]}"#,
                )),
                true,
            ))
            .await
            .unwrap();
        assert_eq!(get(&r, "status").as_f64(), Some(201.0));
        assert!(leader.is_leader());
        let follower = MoulWorker::new(&db).unwrap();
        let subscribe = |collection: &str| {
            obj(&[
                ("op", "subscribe".into()),
                ("collection", collection.into()),
                ("owner", true.into()),
            ])
        };
        let own = leader.request(subscribe("notes")).await.unwrap();
        assert_eq!(get(&own, "ok").as_bool(), Some(true));
        assert_eq!(get(&own, "leader").as_string(), Some(leader.tab_id()));
        let forwarded = follower.request(subscribe("notes")).await.unwrap();
        assert!(!follower.is_leader());
        assert_eq!(get(&forwarded, "ok").as_bool(), Some(true));
        assert_eq!(get(&forwarded, "leader").as_string(), Some(leader.tab_id()));
        assert_ne!(get(&forwarded, "id").as_f64(), get(&own, "id").as_f64());
        // A refused subscription names no tab.
        let refused = follower.request(subscribe("nope")).await.unwrap();
        assert_eq!(get(&refused, "ok").as_bool(), Some(false));
        assert!(get(&refused, "leader").is_undefined());
        follower.close();
        leader.close();
        sleep(100).await;
        OpfsStorage::remove(&host::opfs_path(&db)).await.unwrap();
    }
}
