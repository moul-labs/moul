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
