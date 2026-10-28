//! Save-slot persistence for the custom `.dat` blobs produced by
//! `Game::to_bytes`. Every slot is a player-chosen name holding one world, and
//! there can be as many slots as the storage holds. Native builds keep one
//! file per slot in a `saves/` directory; web builds use IndexedDB (async,
//! large quota) with one record per slot.
//!
//! Because IndexedDB is asynchronous, the web backend keeps an in-memory map
//! of every slot: `init()` kicks off a background read into the map, and
//! `slots()` / `read()` observe it synchronously from the game loop.

/// Clean a player-typed slot name into one safe to store under: reduced to
/// letters, digits, spaces, dashes and underscores (so it doubles as a file
/// name), capped at 40 characters, and trimmed. `None` if nothing survives.
pub fn sanitize_name(raw: &str) -> Option<String> {
    let cleaned: String = raw
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .take(40)
        .collect();
    let cleaned = cleaned.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_string())
    }
}

#[cfg(not(target_arch = "wasm32"))]
mod backend {
    use std::path::PathBuf;

    /// Directory that holds the save files, under the platform's data dir
    /// (e.g. `~/.local/share/kingdom`, `~/Library/Application Support/kingdom`,
    /// `%APPDATA%\kingdom`). Falls back to the current directory if the data
    /// dir can't be resolved.
    fn dir() -> PathBuf {
        dirs::data_dir()
            .map(|d| d.join("kingdom"))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Directory holding one `.dat` file per save slot.
    fn saves_dir() -> PathBuf {
        dir().join("saves")
    }

    fn path(name: &str) -> PathBuf {
        saves_dir().join(format!("{name}.dat"))
    }

    /// Move the single pre-slot save file, if one is still around, into the
    /// saves directory as a slot named "world" (or the first free variation).
    pub fn init() {
        let legacy = dir().join("kingdom_save.dat");
        if !legacy.exists() {
            return;
        }
        if let Err(e) = std::fs::create_dir_all(saves_dir()) {
            log::error!(
                "could not create {} to migrate the old save: {e}",
                saves_dir().display()
            );
            return;
        }
        let mut dest = path("world");
        let mut i = 2;
        while dest.exists() {
            dest = path(&format!("world {i}"));
            i += 1;
        }
        if std::fs::rename(&legacy, &dest).is_err() {
            // A rename can fail across filesystems; fall back to copy + remove.
            if std::fs::copy(&legacy, &dest).is_err() {
                log::error!("could not migrate the old save into {}", dest.display());
                return;
            }
            let _ = std::fs::remove_file(&legacy);
        }
        log::info!("migrated the old save to {}", dest.display());
    }

    pub fn slots() -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(entries) = std::fs::read_dir(saves_dir()) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|x| x.to_str()) == Some("dat")
                    && let Some(stem) = p.file_stem().and_then(|s| s.to_str())
                {
                    out.push(stem.to_string());
                }
            }
        }
        out
    }

    pub fn read(name: &str) -> Option<Vec<u8>> {
        std::fs::read(path(name)).ok()
    }

    pub fn write(name: &str, bytes: Vec<u8>) {
        if let Err(e) = std::fs::create_dir_all(saves_dir()) {
            log::error!(
                "save failed: could not create {}: {e}",
                saves_dir().display()
            );
            return;
        }
        if let Err(e) = std::fs::write(path(name), bytes) {
            log::error!("save failed: {e}");
        } else {
            log::info!("game saved to {}", path(name).display());
        }
    }

    pub fn delete(name: &str) {
        if let Err(e) = std::fs::remove_file(path(name)) {
            log::error!("could not delete save '{name}': {e}");
        } else {
            log::info!("deleted save '{name}'");
        }
    }
}

#[cfg(target_arch = "wasm32")]
mod backend {
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    use wasm_bindgen::JsCast;
    use wasm_bindgen::prelude::*;

    const DB_NAME: &str = "kingdom_db";
    const STORE: &str = "saves";

    thread_local! {
        /// Every slot, mirrored in memory so the game loop can read them
        /// synchronously. The single pre-slot save was stored under the key
        /// "world", so it simply shows up as a slot of that name.
        static SLOTS: RefCell<BTreeMap<String, Vec<u8>>> =
            const { RefCell::new(BTreeMap::new()) };
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
                && let Ok(result) = req.result()
                && let Ok(db) = result.dyn_into::<web_sys::IdbDatabase>()
            {
                let _ = db.create_object_store(STORE);
            }
        });
        open.set_onupgradeneeded(Some(upgrade.as_ref().unchecked_ref()));
        upgrade.forget();

        let open_clone = open.clone();
        let mut f_opt = Some(f);
        let onsuccess = Closure::<dyn FnMut()>::new(move || {
            if let Ok(result) = open_clone.result()
                && let Ok(db) = result.dyn_into::<web_sys::IdbDatabase>()
                && let Some(f) = f_opt.take()
            {
                f(db);
            }
        });
        open.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
        onsuccess.forget();
    }

    /// Walk the whole store with a cursor and mirror every slot into `SLOTS`.
    pub fn init() {
        with_db(|db| {
            let Ok(tx) = db.transaction_with_str(STORE) else {
                return;
            };
            let Ok(store) = tx.object_store(STORE) else {
                return;
            };
            let Ok(req) = store.open_cursor() else {
                return;
            };
            let req_clone = req.clone();
            let onsuccess = Closure::<dyn FnMut()>::new(move || {
                let Ok(result) = req_clone.result() else {
                    return;
                };
                // A null result means the cursor has walked past the last
                // record, so the cast failing ends the iteration.
                let Ok(cursor) = result.dyn_into::<web_sys::IdbCursorWithValue>() else {
                    return;
                };
                if let (Ok(key), Ok(value)) = (cursor.key(), cursor.value())
                    && let Some(name) = key.as_string()
                    && let Ok(arr) = value.dyn_into::<js_sys::Uint8Array>()
                {
                    let bytes = arr.to_vec();
                    SLOTS.with(|s| {
                        s.borrow_mut().insert(name, bytes);
                    });
                }
                let _ = cursor.continue_();
            });
            req.set_onsuccess(Some(onsuccess.as_ref().unchecked_ref()));
            onsuccess.forget();
        });
    }

    pub fn slots() -> Vec<String> {
        SLOTS.with(|s| s.borrow().keys().cloned().collect())
    }

    pub fn read(name: &str) -> Option<Vec<u8>> {
        SLOTS.with(|s| s.borrow().get(name).cloned())
    }

    pub fn write(name: &str, bytes: Vec<u8>) {
        // Update the in-memory map immediately so a subsequent load works.
        SLOTS.with(|s| {
            s.borrow_mut().insert(name.to_string(), bytes.clone());
        });
        let key = name.to_string();
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
            let _ = store.put_with_key(&arr, &JsValue::from_str(&key));
        });
        log::info!("game saved to IndexedDB slot '{name}'");
    }

    pub fn delete(name: &str) {
        SLOTS.with(|s| {
            s.borrow_mut().remove(name);
        });
        let key = name.to_string();
        with_db(move |db| {
            let Ok(tx) =
                db.transaction_with_str_and_mode(STORE, web_sys::IdbTransactionMode::Readwrite)
            else {
                return;
            };
            let Ok(store) = tx.object_store(STORE) else {
                return;
            };
            let _ = store.delete(&JsValue::from_str(&key));
        });
        log::info!("deleted IndexedDB save slot '{name}'");
    }
}

/// Begin loading any existing saves (async on web; migrates the old
/// single-file save into a slot on native).
pub fn init() {
    backend::init();
}

/// Every save slot's name, sorted for a stable menu.
pub fn slots() -> Vec<String> {
    let mut names = backend::slots();
    names.sort_by_key(|n| n.to_lowercase());
    names
}

/// Read a slot's raw save blob, if present.
pub fn read(name: &str) -> Option<Vec<u8>> {
    backend::read(name)
}

/// Persist a raw save blob under a slot name (already sanitized).
pub fn write(name: &str, bytes: Vec<u8>) {
    backend::write(name, bytes);
}

/// Remove a save slot.
pub fn delete(name: &str) {
    backend::delete(name);
}

#[cfg(test)]
mod tests {
    use super::sanitize_name;

    #[test]
    fn sanitize_keeps_ordinary_names() {
        assert_eq!(sanitize_name("My Kingdom 2"), Some("My Kingdom 2".into()));
        assert_eq!(sanitize_name("world -42"), Some("world -42".into()));
    }

    #[test]
    fn sanitize_strips_path_characters() {
        assert_eq!(sanitize_name("../evil"), Some("evil".into()));
        assert_eq!(sanitize_name("a/b\\c"), Some("abc".into()));
    }

    #[test]
    fn sanitize_rejects_empty_results() {
        assert_eq!(sanitize_name(""), None);
        assert_eq!(sanitize_name("   "), None);
        assert_eq!(sanitize_name("./.."), None);
    }

    #[test]
    fn sanitize_caps_length() {
        let long = "x".repeat(100);
        assert_eq!(sanitize_name(&long).unwrap().chars().count(), 40);
    }
}
