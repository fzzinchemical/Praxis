//! VS Code webview UI, compiled to wasm32 (SPEC §11).
//!
//! T0.6 spike: appends text messages from the extension to the output element.

use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::MessageEvent;

#[wasm_bindgen]
extern "C" {
    type VsCodeApi;
    #[wasm_bindgen(js_name = acquireVsCodeApi)]
    fn acquire_vscode_api() -> VsCodeApi;
    #[wasm_bindgen(method, js_name = postMessage)]
    fn post_message(this: &VsCodeApi, message: &str);
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let output = document.get_element_by_id("output").ok_or("no #output")?;

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        // Text only, never innerHTML: process output is untrusted.
        if let Some(text) = event.data().as_string() {
            let shown = output.text_content().unwrap_or_default();
            output.set_text_content(Some(&(shown + &text)));
        }
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())?;
    // The listener lives as long as the page.
    on_message.forget();

    acquire_vscode_api().post_message("ready");
    Ok(())
}
