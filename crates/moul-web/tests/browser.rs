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

    fn echo_leader() -> Box<dyn FnMut(JsValue) -> Result<JsValue, JsValue>> {
        let mut count = 0;
        Box::new(move |body| {
            count += 1;
            if get(&body, "fail").as_bool() == Some(true) {
                return Err(js_sys::Error::new("refused").into());
            }
            Ok(obj(&[("echo", body), ("count", count.into())]))
        })
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
        let engine = Engine::new(opened, Some(Box::new(move || w.set(w.get() + 1))));

        let req = |pairs: &[(&str, JsValue)]| obj(pairs);
        engine
            .handle(req(&[
                ("op", "exec".into()),
                ("sql", "CREATE TABLE t (a, b)".into()),
            ]))
            .unwrap();
        let params = js_sys::Array::of3(&1.into(), &"two".into(), &Uint8Array::from(&[3u8, 4][..]));
        engine
            .handle(req(&[
                ("op", "exec".into()),
                ("sql", "INSERT INTO t VALUES (?1, ?2), (99, ?3)".into()),
                ("params", params.into()),
            ]))
            .unwrap();
        assert_eq!(wrote.get(), 1, "the first-write hook runs once");

        let q = engine
            .handle(req(&[
                ("op", "query".into()),
                ("sql", "SELECT a, b FROM t ORDER BY a".into()),
            ]))
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
            .handle(req(&[("op", "export".into())]))
            .unwrap()
            .unchecked_into();
        engine
            .handle(req(&[
                ("op", "exec".into()),
                ("sql", "DELETE FROM t".into()),
            ]))
            .unwrap();
        engine
            .handle(req(&[("op", "import".into()), ("bytes", exported.into())]))
            .unwrap();
        let q = engine
            .handle(req(&[
                ("op", "query".into()),
                ("sql", "SELECT count(*) FROM t".into()),
            ]))
            .unwrap();
        let rows: js_sys::Array = get(&q, "rows").unchecked_into();
        let row: js_sys::Array = rows.get(0).unchecked_into();
        assert_eq!(row.get(0).as_f64(), Some(2.0));

        let status = engine.handle(req(&[("op", "status".into())])).unwrap();
        assert_eq!(get(&status, "durable").as_bool(), Some(true));
        assert!(engine.handle(req(&[("op", "nope".into())])).is_err());
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
}
