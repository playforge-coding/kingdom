//! Save-file persistence for the custom `.dat` blob produced by
//! `Game::to_bytes`. Native builds read/write a file; web builds use IndexedDB
//! (async, large quota, stores the blob directly).
//!
//! Because IndexedDB is asynchronous, the web backend keeps a small in-memory
//! slot: `init()` kicks off a background read into the slot, and `exists()` /
//! `read()` observe it synchronously from the game loop.

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use std::path::PathBuf;

    /// Directory that holds the save file, under the platform's data dir
    /// (e.g. `~/.local/share/kingdom`, `~/Library/Application Support/kingdom`,
    /// `%APPDATA%\kingdom`). Falls back to the current directory if the data
    /// dir can't be resolved.
    fn dir() -> PathBuf {
        dirs::data_dir()
            .map(|d| d.join("kingdom"))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    fn path() -> PathBuf {
        dir().join("kingdom_save.dat")
    }

    pub fn init() {}

    pub fn exists() -> bool {
        path().exists()
    }

    pub fn read() -> Option<Vec<u8>> {
        std::fs::read(path()).ok()
    }

    pub fn write(bytes: Vec<u8>) {
        if let Err(e) = std::fs::create_dir_all(dir()) {
            log::error!("save failed: could not create {}: {e}", dir().display());
            return;
        }
        if let Err(e) = std::fs::write(path(), bytes) {
            log::error!("save failed: {e}");
        } else {
            log::info!("game saved to {}", path().display());
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod backend {
    use std::cell::RefCell;

    use wasm_bindgen::prelude::*;
    use wasm_bindgen::JsCast;

    const DB_NAME: &str = "kingdom_db";
    const STORE: &str = "saves";
    const KEY: &str = "world";

    thread_local! {
        static SLOT: RefCell<Option<Vec<u8>>> = const { RefCell::new(None) };
    }

    /// Open the database (creating the object store on first use) and hand the
    /// ready `IdbDatabase` to `f`.
    fn with_db<F>(f: F)
    where
        F: FnOnce(web_sys::IdbDatabase) + 'static,
    {
        let Some(win) = web_sys::window() else { return };
        let factory = match win.indexed_db() {
            Ok(Some(f)) => f,
            _ => return,
        };
        let open = match factory.open_with_u32(DB_NAME, 1) {
            Ok(o) => o,
            Err(_) => return,
        };

        // Create the object store during an upgrade.
        let upgrade = Closure::<dyn FnMut(web_sys::Event)>::new(move |e: web_sys::Event| {
            if let Some(req) = e
                .target()
                .and_then(|t| t.dyn_into::<web_sys::IdbOpenDbRequest>().ok())
            {
                if let Ok(result) = req.result() {
                    if let Ok(db) = result.dyn_into::<web_sys::IdbDatabase>() {
                        let _ = db.create_object_store(STORE);
                    }
                }
            }
        });
        open.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
        upgrade.forget();

        let open_clone = open.clone();
        let mut f_opt = Some(f);
        let onsuccess = Closure::<dyn FnMut()>::new(move || {
            if let Ok(result) = open_clone.result() {
                if let Ok(db) = result.dyn_into::<web_sys::IdbDatabase>() {
                    if let Some(f) = f_opt.take() {
                        f(db);
                    }
                }
            }
        });
        open.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
        onsuccess.forget();
    }

    pub fn init() {
        with_db(|db| {
            let Ok(tx) = db.transaction_with_str(STORE) else {
                return;
            };
            let Ok(store) = tx.object_store(STORE) else {
                return;
            };
            let Ok(req) = store.get(&JsValue::from_str(KEY)) else {
                return;
            };
            let req_clone = req.clone();
            let onsuccess = Closure::<dyn FnMut()>::new(move || {
                if let Ok(result) = req_clone.result() {
                    if let Ok(arr) = result.dyn_into::<js_sys::Uint8Array>() {
                        let bytes = arr.to_vec();
                        SLOT.with(|s| *s.borrow_mut() = Some(bytes));
                    }
                }
            });
            req.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
            onsuccess.forget();
        });
    }

    pub fn exists() -> bool {
        SLOT.with(|s| s.borrow().is_some())
    }

    pub fn read() -> Option<Vec<u8>> {
        SLOT.with(|s| s.borrow().clone())
    }

    pub fn write(bytes: Vec<u8>) {
        // Update the in-memory slot immediately so a subsequent load works.
        SLOT.with(|s| *s.borrow_mut() = Some(bytes.clone()));
        with_db(move |db| {
            let Ok(tx) =
                db.transaction_with_str_and_mode(STORE, web_sys::IdbTransactionMode::Readwrite)
            else {
                return;
            };
            let Ok(store) = tx.object_store(STORE) else {
                return;
            };
            let arr = js_sys::Uint8Array::from(bytes.as_slice());
            let _ = store.put_with_key(&arr, &JsValue::from_str(KEY));
        });
        log::info!("game saved to IndexedDB");
    }
}

/// Begin loading any existing save (async on web; no-op on native).
pub fn init() {
    backend::init();
}

/// Is a save available to load?
pub fn exists() -> bool {
    backend::exists()
}

/// Read the raw save blob, if present.
pub fn read() -> Option<Vec<u8>> {
    backend::read()
}

/// Persist the raw save blob.
pub fn write(bytes: Vec<u8>) {
    backend::write(bytes);
}
