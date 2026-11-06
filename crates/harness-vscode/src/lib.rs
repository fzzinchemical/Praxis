//! VS Code extension host side, compiled to wasm32 (SPEC §11).
//!
//! T0.6 spike: a command opens a webview, and once the webview's wasm reports
//! ready, a child process is spawned and its output is forwarded to the webview.

use js_sys::{Array, Object, Reflect};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

const SHELL: &str = include_str!("../../../editors/vscode/webview.html");
const VIEW_COLUMN_ACTIVE: i32 = -1;

#[wasm_bindgen]
extern "C" {
    pub type ExtensionContext;
    #[wasm_bindgen(method, getter, js_name = extensionUri)]
    fn extension_uri(this: &ExtensionContext) -> Uri;
    #[wasm_bindgen(method, getter)]
    fn subscriptions(this: &ExtensionContext) -> Array;
}

#[wasm_bindgen(module = "vscode")]
extern "C" {
    #[wasm_bindgen(catch, js_namespace = commands, js_name = registerCommand)]
    fn register_command(id: &str, callback: &Closure<dyn FnMut()>) -> Result<JsValue, JsValue>;

    #[wasm_bindgen(catch, js_namespace = window, js_name = createWebviewPanel)]
    fn create_webview_panel(
        view_type: &str,
        title: &str,
        column: i32,
        options: &Object,
    ) -> Result<WebviewPanel, JsValue>;

    #[wasm_bindgen(js_namespace = window, js_name = showErrorMessage)]
    fn show_error_message(message: &str);

    type Uri;
    #[wasm_bindgen(static_method_of = Uri, js_name = joinPath)]
    fn join_path(base: &Uri, path: &str) -> Uri;
    #[wasm_bindgen(method, js_name = toString)]
    fn to_uri_string(this: &Uri) -> String;

    type WebviewPanel;
    #[wasm_bindgen(method, getter)]
    fn webview(this: &WebviewPanel) -> Webview;

    #[derive(Clone)]
    type Webview;
    #[wasm_bindgen(method, getter, js_name = cspSource)]
    fn csp_source(this: &Webview) -> String;
    #[wasm_bindgen(method, setter)]
    fn set_html(this: &Webview, html: &str);
    #[wasm_bindgen(method, js_name = asWebviewUri)]
    fn as_webview_uri(this: &Webview, uri: &Uri) -> Uri;
    #[wasm_bindgen(method, js_name = postMessage)]
    fn post_message(this: &Webview, message: &str);
    #[wasm_bindgen(method, js_name = onDidReceiveMessage)]
    fn on_did_receive_message(this: &Webview, callback: &Closure<dyn FnMut(JsValue)>);
}

#[wasm_bindgen(module = "child_process")]
extern "C" {
    type ChildProcess;
    #[wasm_bindgen(catch)]
    fn spawn(command: &str, args: &Array) -> Result<ChildProcess, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn stdout(this: &ChildProcess) -> Emitter;
    #[wasm_bindgen(method, getter)]
    fn stderr(this: &ChildProcess) -> Emitter;
    #[wasm_bindgen(method, js_name = on)]
    fn on_child(this: &ChildProcess, event: &str, callback: &Closure<dyn FnMut(JsValue)>);

    type Emitter;
    #[wasm_bindgen(method, js_name = setEncoding)]
    fn set_encoding(this: &Emitter, encoding: &str);
    #[wasm_bindgen(method)]
    fn on(this: &Emitter, event: &str, callback: &Closure<dyn FnMut(JsValue)>);
}

/// Entry point VS Code calls. An `Err` is thrown, which fails activation visibly.
#[wasm_bindgen]
pub fn activate(context: ExtensionContext) -> Result<(), JsValue> {
    let root = context.extension_uri();
    let command = Closure::<dyn FnMut()>::new(move || {
        if let Err(error) = open_panel(&root) {
            show_error_message(&format!("Harness spike failed: {error:?}"));
        }
    });
    let disposable = register_command("harness.spike", &command)?;
    context.subscriptions().push(&disposable);
    // shortcut: closures in this file leak until the host exits, tie them to disposables in T5.2.
    command.forget();
    Ok(())
}

fn open_panel(root: &Uri) -> Result<(), JsValue> {
    let options = Object::new();
    Reflect::set(&options, &"enableScripts".into(), &true.into())?;
    let roots = Array::of1(root.unchecked_ref());
    Reflect::set(&options, &"localResourceRoots".into(), &roots)?;
    let panel = create_webview_panel("harness.spike", "Harness", VIEW_COLUMN_ACTIVE, &options)?;
    let webview = panel.webview();

    let target = webview.clone();
    let on_message = Closure::<dyn FnMut(JsValue)>::new(move |message: JsValue| {
        // The webview is untrusted: only the exact ready message starts a process.
        if message.as_string().as_deref() == Some("ready") {
            run_child(&target);
        }
    });
    webview.on_did_receive_message(&on_message);
    on_message.forget();

    let script = webview.as_webview_uri(&Uri::join_path(root, "ui/harness_vscode_ui.js"));
    webview.set_html(
        &SHELL
            .replace("{{cspSource}}", &webview.csp_source())
            .replace("{{uiScript}}", &script.to_uri_string()),
    );
    Ok(())
}

fn run_child(webview: &Webview) {
    let args = Array::of1(&"-sr".into());
    let child = match spawn("uname", &args) {
        Ok(child) => child,
        Err(error) => return webview.post_message(&format!("spawn failed: {error:?}\n")),
    };
    for stream in [child.stdout(), child.stderr()] {
        stream.set_encoding("utf8");
        let listener = forwarder(webview, |chunk| chunk);
        stream.on("data", &listener);
        listener.forget();
    }
    // Node emits `error` for a missing binary; unhandled, it would crash the extension host.
    let on_error = forwarder(webview, |error| format!("child error: {error}\n"));
    child.on_child("error", &on_error);
    on_error.forget();
    let on_close = forwarder(webview, |code| format!("[exit {code}]\n"));
    child.on_child("close", &on_close);
    on_close.forget();
}

/// Builds a listener that posts `render(value)` to the webview.
fn forwarder(webview: &Webview, render: fn(String) -> String) -> Closure<dyn FnMut(JsValue)> {
    let webview = webview.clone();
    Closure::new(move |value: JsValue| {
        let text = value
            .as_string()
            .or_else(|| value.as_f64().map(|number| number.to_string()))
            .unwrap_or_else(|| format!("{value:?}"));
        webview.post_message(&render(text));
    })
}
