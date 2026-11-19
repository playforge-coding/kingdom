//! The system clipboard for the HUD's text boxes in the browser.
//!
//! On the web `egui-winit` has no clipboard backend: Cmd/Ctrl+V pastes only
//! text copied from inside the game, and copying never reaches the system
//! clipboard. The browser's own `paste` event can't step in either, because
//! winit calls `preventDefault` on every keydown on the canvas, which cancels
//! the paste before it happens.
//!
//! So a capture-phase listener on the window catches the paste shortcut before
//! winit's canvas listener runs and stops it there. The browser then fires its
//! `paste` event as usual and the text it carries is queued for egui as an
//! [`egui::Event::Paste`]. Reading the clipboard this way needs no permission
//! prompt, unlike `navigator.clipboard.readText()`. In the other direction,
//! text egui copies goes out through `navigator.clipboard.writeText()`.

use std::cell::RefCell;
use std::rc::Rc;

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

pub struct WebClipboard {
    pasted: Rc<RefCell<Vec<String>>>,
}

impl WebClipboard {
    /// Install the listeners. They live as long as the page, so the closures
    /// are leaked on purpose.
    pub fn install() -> Self {
        let pasted = Rc::new(RefCell::new(Vec::new()));
        let Some(window) = web_sys::window() else {
            return WebClipboard { pasted };
        };

        let on_keydown =
            Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(|e: web_sys::KeyboardEvent| {
                if is_paste_shortcut(&e) {
                    // Keep winit from seeing (and cancelling) the keypress.
                    e.stop_immediate_propagation();
                }
            });
        window
            .add_event_listener_with_callback_and_bool(
                "keydown",
                on_keydown.as_ref().unchecked_ref(),
                true,
            )
            .ok();
        on_keydown.forget();

        let queue = Rc::clone(&pasted);
        let on_paste = Closure::<dyn FnMut(web_sys::ClipboardEvent)>::new(
            move |e: web_sys::ClipboardEvent| {
                let Some(data) = e.clipboard_data() else {
                    return;
                };
                if let Ok(text) = data.get_data("text/plain") {
                    let text = text.replace("\r\n", "\n");
                    if !text.is_empty() {
                        queue.borrow_mut().push(text);
                    }
                }
                e.prevent_default();
            },
        );
        window
            .add_event_listener_with_callback("paste", on_paste.as_ref().unchecked_ref())
            .ok();
        on_paste.forget();

        WebClipboard { pasted }
    }

    /// Hand any text pasted since the last frame to egui.
    pub fn feed(&self, input: &mut egui::RawInput) {
        input
            .events
            .extend(self.pasted.borrow_mut().drain(..).map(egui::Event::Paste));
    }

    /// Send text egui copied or cut to the system clipboard.
    pub fn copy(&self, text: &str) {
        if let Some(window) = web_sys::window() {
            // The promise rejects if the page lost focus; nothing to do then.
            let _ = window.navigator().clipboard().write_text(text);
        }
    }
}

/// Cmd+V on macOS, Ctrl+V elsewhere, and Shift+Insert.
fn is_paste_shortcut(e: &web_sys::KeyboardEvent) -> bool {
    let v = e.code() == "KeyV" || e.key().eq_ignore_ascii_case("v");
    ((e.ctrl_key() || e.meta_key()) && !e.alt_key() && v) || (e.shift_key() && e.key() == "Insert")
}
