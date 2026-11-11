//! VS Code webview UI, compiled to wasm32 (SPEC §11).
//!
//! Appends text from the extension to the transcript and sends the prompt form as a typed message.

use serde_json::json;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{Event, HtmlTextAreaElement, MessageEvent};

#[wasm_bindgen]
extern "C" {
    #[derive(Clone)]
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
    let form = document
        .get_element_by_id("prompt-form")
        .ok_or("no #prompt-form")?;
    let input: HtmlTextAreaElement = document
        .get_element_by_id("prompt")
        .ok_or("no #prompt")?
        .dyn_into()?;
    let vscode = acquire_vscode_api();

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        // Text only, never innerHTML: agent output is untrusted.
        if let Some(text) = event.data().as_string() {
            let shown = output.text_content().unwrap_or_default();
            output.set_text_content(Some(&(shown + &text)));
        }
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())?;

    let api = vscode.clone();
    let on_submit = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        let text = input.value();
        if !text.trim().is_empty() {
            api.post_message(&json!({"type": "prompt", "text": text}).to_string());
            input.set_value("");
        }
    });
    form.add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref())?;
    // Both listeners live as long as the page.
    on_message.forget();
    on_submit.forget();

    vscode.post_message(&json!({"type": "ready"}).to_string());
    Ok(())
}
