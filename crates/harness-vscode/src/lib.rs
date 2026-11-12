//! VS Code extension host side, compiled to wasm32 (SPEC §11).
//!
//! `harness.open` opens a chat panel. Once its wasm reports ready, `harness acp` is spawned in the
//! first workspace folder; prompts, stop and permission answers go to it, typed updates come back.

mod protocol;

use js_sys::{Array, Object, Promise, Reflect};
use protocol::{Client, FromPanel, Lines, Reaction, Review};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

const SHELL: &str = include_str!("../../../editors/vscode/webview.html");
const VIEW_COLUMN_ACTIVE: i32 = -1;
const RESTART: &str = "Restart";

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
    #[wasm_bindgen(js_namespace = window, js_name = showErrorMessage)]
    fn show_error_with_action(message: &str, action: &str) -> Promise;

    #[wasm_bindgen(js_namespace = workspace, js_name = openTextDocument)]
    fn open_text_document(options: &Object) -> Promise;
    #[wasm_bindgen(js_namespace = commands, js_name = executeCommand)]
    fn execute_command(command: &str, left: &Uri, right: &Uri, title: &str) -> Promise;
    #[wasm_bindgen(js_namespace = window, js_name = createOutputChannel)]
    fn create_output_channel(name: &str) -> OutputChannel;
    #[derive(Clone)]
    type OutputChannel;
    #[wasm_bindgen(method)]
    fn append(this: &OutputChannel, text: &str);

    #[wasm_bindgen(js_namespace = workspace, js_name = getConfiguration)]
    fn get_configuration(section: &str) -> Configuration;
    type Configuration;
    #[wasm_bindgen(method)]
    fn get(this: &Configuration, key: &str) -> JsValue;
    // A `getter` on a namespaced free function is emitted as a call (`workspace.workspaceFolders()`),
    // which throws; bind the namespace object and read the property instead.
    #[wasm_bindgen(thread_local_v2, js_name = workspace)]
    static WORKSPACE: JsValue;

    type Uri;
    #[wasm_bindgen(static_method_of = Uri, js_name = joinPath)]
    fn join_path(base: &Uri, path: &str) -> Uri;
    #[wasm_bindgen(method, js_name = toString)]
    fn to_uri_string(this: &Uri) -> String;
    #[wasm_bindgen(method, getter, js_name = fsPath)]
    fn fs_path(this: &Uri) -> String;

    /// An open editor document; `getText` includes unsaved edits.
    type TextDocument;
    #[wasm_bindgen(method, getter)]
    fn uri(this: &TextDocument) -> Uri;
    #[wasm_bindgen(method, js_name = getText)]
    fn get_text(this: &TextDocument) -> String;

    type WebviewPanel;
    #[wasm_bindgen(method, getter)]
    fn webview(this: &WebviewPanel) -> Webview;
    #[wasm_bindgen(method, js_name = onDidDispose)]
    fn on_did_dispose(this: &WebviewPanel, callback: &Closure<dyn FnMut()>);

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

#[wasm_bindgen(module = "fs")]
extern "C" {
    #[wasm_bindgen(catch, js_name = readFileSync)]
    fn read_file_sync(path: &str, encoding: &str) -> Result<JsValue, JsValue>;
}

#[wasm_bindgen(module = "child_process")]
extern "C" {
    type ChildProcess;
    #[wasm_bindgen(catch)]
    fn spawn(command: &str, args: &Array, options: &Object) -> Result<ChildProcess, JsValue>;
    #[wasm_bindgen(method, getter)]
    fn stdin(this: &ChildProcess) -> Writable;
    #[wasm_bindgen(method, getter)]
    fn stdout(this: &ChildProcess) -> Emitter;
    #[wasm_bindgen(method, getter)]
    fn stderr(this: &ChildProcess) -> Emitter;
    #[wasm_bindgen(method, js_name = on)]
    fn on_child(this: &ChildProcess, event: &str, callback: &Closure<dyn FnMut(JsValue)>);
    #[wasm_bindgen(method)]
    fn kill(this: &ChildProcess) -> bool;

    type Writable;
    #[wasm_bindgen(method)]
    fn write(this: &Writable, text: &str) -> bool;

    type Emitter;
    #[wasm_bindgen(method, js_name = setEncoding)]
    fn set_encoding(this: &Emitter, encoding: &str);
    #[wasm_bindgen(method)]
    fn on(this: &Emitter, event: &str, callback: &Closure<dyn FnMut(JsValue)>);
}

impl Webview {
    fn notify(&self, text: &str) {
        self.post_message(&protocol::notice(text).to_string());
    }
}

/// One panel and the `harness acp` process behind it.
struct Panel {
    webview: Webview,
    output: OutputChannel,
    child: Option<ChildProcess>,
    client: Option<Client>,
    lines: Lines,
    closed: bool,
}

impl Panel {
    /// Writes each line to the agent's stdin, if it is running.
    fn send(&self, lines: impl IntoIterator<Item = String>) {
        if let Some(child) = &self.child {
            for line in lines {
                child.stdin().write(&format!("{line}\n"));
            }
        }
    }
}

type Shared = Rc<RefCell<Panel>>;

/// Entry point VS Code calls. An `Err` is thrown, which fails activation visibly.
#[wasm_bindgen]
pub fn activate(context: ExtensionContext) -> Result<(), JsValue> {
    let root = context.extension_uri();
    let output = create_output_channel("Harness");
    let command = Closure::<dyn FnMut()>::new(move || {
        if let Err(error) = open_panel(&root, &output) {
            show_error_message(&format!("Harness: could not open the panel: {error:?}"));
        }
    });
    let disposable = register_command("harness.open", &command)?;
    context.subscriptions().push(&disposable);
    // shortcut: closures leak until the host exits; fine for one command, revisit if they multiply.
    command.forget();
    Ok(())
}

fn open_panel(root: &Uri, output: &OutputChannel) -> Result<(), JsValue> {
    let options = Object::new();
    Reflect::set(&options, &"enableScripts".into(), &true.into())?;
    let roots = Array::of1(root.unchecked_ref());
    Reflect::set(&options, &"localResourceRoots".into(), &roots)?;
    let panel = create_webview_panel("harness.chat", "Harness", VIEW_COLUMN_ACTIVE, &options)?;
    let webview = panel.webview();
    let shared: Shared = Rc::new(RefCell::new(Panel {
        webview: webview.clone(),
        output: output.clone(),
        child: None,
        client: None,
        lines: Lines::default(),
        closed: false,
    }));

    let state = shared.clone();
    let on_message = Closure::<dyn FnMut(JsValue)>::new(move |message: JsValue| {
        match message.as_string().as_deref().and_then(FromPanel::parse) {
            Some(FromPanel::Ready) => start(&state),
            Some(FromPanel::Prompt(text)) => send_prompt(&state, &text),
            Some(FromPanel::Cancel) => {
                let mut panel = state.borrow_mut();
                let lines = panel.client.as_mut().map(Client::cancel);
                panel.send(lines.unwrap_or_default());
            }
            Some(FromPanel::Permission(id, option)) => {
                let mut panel = state.borrow_mut();
                let line = panel
                    .client
                    .as_mut()
                    .and_then(|client| client.answer(&id, option.as_deref()));
                panel.send(line);
            }
            None => state
                .borrow()
                .output
                .append("ignored an unknown panel message\n"),
        }
    });
    webview.on_did_receive_message(&on_message);
    on_message.forget();

    let state = shared;
    let on_dispose = Closure::<dyn FnMut()>::new(move || {
        let mut panel = state.borrow_mut();
        panel.closed = true;
        if let Some(child) = panel.child.take() {
            child.kill();
        }
    });
    panel.on_did_dispose(&on_dispose);
    on_dispose.forget();

    let script = webview.as_webview_uri(&Uri::join_path(root, "ui/harness_vscode_ui.js"));
    let style = webview.as_webview_uri(&Uri::join_path(root, "ui/ui.css"));
    webview.set_html(
        &SHELL
            .replace("{{cspSource}}", &webview.csp_source())
            .replace("{{uiScript}}", &script.to_uri_string())
            .replace("{{uiStyle}}", &style.to_uri_string()),
    );
    Ok(())
}

/// The first workspace folder: `harness acp` runs there and reads its project config.
fn workspace_dir() -> Option<String> {
    let folders = WORKSPACE.with(|workspace| Reflect::get(workspace, &"workspaceFolders".into()));
    let folder = Reflect::get(&folders.ok()?, &0.into()).ok()?;
    let uri = Reflect::get(&folder, &"uri".into()).ok()?;
    Reflect::get(&uri, &"fsPath".into()).ok()?.as_string()
}

fn start(state: &Shared) {
    let Some(cwd) = workspace_dir() else {
        state
            .borrow()
            .webview
            .notify("Open a folder to use Harness.");
        return;
    };
    // `harness.binaryPath` is machine-scoped in package.json, so a repository can't redirect it.
    let binary = get_configuration("harness")
        .get("binaryPath")
        .as_string()
        .unwrap_or_else(|| "harness".to_owned());
    let options = Object::new();
    if let Err(error) = Reflect::set(&options, &"cwd".into(), &cwd.as_str().into()) {
        return state
            .borrow()
            .webview
            .notify(&format!("spawn failed: {error:?}"));
    }
    let child = match spawn(&binary, &Array::of1(&"acp".into()), &options) {
        Ok(child) => child,
        Err(error) => {
            return state
                .borrow()
                .webview
                .notify(&format!("spawn failed: {error:?}"));
        }
    };
    let (client, init) = Client::start(&cwd);
    child.stdin().write(&format!("{init}\n"));
    child.stdout().set_encoding("utf8");
    child.stderr().set_encoding("utf8");

    let on_stdout = listener(state, |state, chunk| {
        let lines = state.borrow_mut().lines.push(&chunk);
        for line in lines {
            let reaction = match state.borrow_mut().client.as_mut() {
                Some(client) => client.on_line(&line),
                None => Reaction::default(),
            };
            react(state, reaction);
        }
    });
    child.stdout().on("data", &on_stdout);
    let on_stderr = listener(state, |state, chunk| state.borrow().output.append(&chunk));
    child.stderr().on("data", &on_stderr);
    // Node emits `error` for a missing binary; unhandled, it would crash the extension host.
    let on_error = listener(state, |state, error| {
        let panel = state.borrow();
        panel.output.append(&format!("harness acp: {error}\n"));
        panel
            .webview
            .notify(&format!("harness acp failed: {error}"));
    });
    child.on_child("error", &on_error);
    let on_close = listener(state, |state, code| exited(state, &code));
    child.on_child("close", &on_close);
    for closure in [on_stdout, on_stderr, on_error, on_close] {
        closure.forget();
    }

    let mut panel = state.borrow_mut();
    panel.child = Some(child);
    panel.client = Some(client);
    panel.lines = Lines::default();
}

fn send_prompt(state: &Shared, text: &str) {
    let mut panel = state.borrow_mut();
    let line = panel.client.as_mut().and_then(|client| client.prompt(text));
    match (line, &panel.child) {
        (Some(line), Some(child)) => {
            child.stdin().write(&format!("{line}\n"));
        }
        _ => panel.webview.notify("Harness is not ready yet."),
    }
}

fn react(state: &Shared, reaction: Reaction) {
    let panel = state.borrow();
    if let Some(message) = reaction.show {
        panel.webview.post_message(&message.to_string());
    }
    if let Some(review) = reaction.review
        && let Err(error) = open_review(&review, &panel.output)
    {
        panel
            .output
            .append(&format!("could not open the diff: {error:?}\n"));
    }
    if let Some(read) = reaction.read {
        let text = editor_text(&read.path);
        panel.send([protocol::file_reply(read.id, text.as_deref())]);
    }
    panel.send(reaction.send);
}

/// Opens VS Code's diff editor on a proposed change. Both sides are untitled documents, so the
/// file on disk is untouched; the agent writes only if the user allows the call.
// shortcut: the untitled sides stay open and unsaved after the answer; a text document content
// provider would let them close cleanly, upgrade if people trip over it.
fn open_review(review: &Review, output: &OutputChannel) -> Result<(), JsValue> {
    let old: JsValue = untitled(&review.old_text)?.into();
    let new: JsValue = untitled(&review.new_text)?.into();
    let sides = Array::of2(&old, &new);
    let (failed_open, failed_diff) = (failure_logger(output), failure_logger(output));
    let title = review.title.clone();
    let on_sides = Closure::<dyn FnMut(JsValue)>::new(move |opened: JsValue| {
        let opened = Array::from(&opened);
        let left = opened.get(0).unchecked_into::<TextDocument>().uri();
        let right = opened.get(1).unchecked_into::<TextDocument>().uri();
        let _ = execute_command("vscode.diff", &left, &right, &title).catch(&failed_diff);
    });
    let _ = Promise::all(&sides).then(&on_sides).catch(&failed_open);
    // Each closure runs once and must outlive its promise, so the host keeps it.
    on_sides.forget();
    failed_open.forget();
    Ok(())
}

/// An untitled document holding `text`, from `workspace.openTextDocument`.
fn untitled(text: &str) -> Result<Promise, JsValue> {
    let options = Object::new();
    Reflect::set(&options, &"content".into(), &text.into())?;
    // shortcut: every side is plain text, so the diff has no syntax colouring.
    Reflect::set(&options, &"language".into(), &"plaintext".into())?;
    Ok(open_text_document(&options))
}

/// Logs a failed step of opening a diff to the output channel.
fn failure_logger(output: &OutputChannel) -> Closure<dyn FnMut(JsValue)> {
    let output = output.clone();
    Closure::new(move |error: JsValue| {
        output.append(&format!("diff review failed: {error:?}\n"));
    })
}

/// The text of an open document at `path`, unsaved edits included, else the file on disk.
fn editor_text(path: &str) -> Option<String> {
    let documents = WORKSPACE.with(|workspace| Reflect::get(workspace, &"textDocuments".into()));
    let documents: Array = documents.ok()?.unchecked_into();
    if let Some(document) = documents
        .iter()
        .map(|value| value.unchecked_into::<TextDocument>())
        .find(|document| document.uri().fs_path() == path)
    {
        return Some(document.get_text());
    }
    // shortcut: reads the whole file into memory with no size cap; cap it if large files show up.
    read_file_sync(path, "utf8").ok()?.as_string()
}

fn exited(state: &Shared, code: &str) {
    {
        let mut panel = state.borrow_mut();
        panel.child = None;
        panel.client = None;
        if panel.closed {
            return;
        }
        panel.webview.notify(&format!("harness acp exited: {code}"));
    }
    let message = format!("Harness exited ({code}). Its log is in the Harness output channel.");
    let state = state.clone();
    let on_choice = Closure::<dyn FnMut(JsValue)>::new(move |choice: JsValue| {
        if choice.as_string().as_deref() == Some(RESTART) && !state.borrow().closed {
            start(&state);
        }
    });
    let _ = show_error_with_action(&message, RESTART).then(&on_choice);
    // One small closure per exit.
    on_choice.forget();
}

/// Wraps `handle` as a Node event listener, turning its argument into text.
fn listener(state: &Shared, handle: fn(&Shared, String)) -> Closure<dyn FnMut(JsValue)> {
    let state = state.clone();
    Closure::new(move |value: JsValue| {
        let text = value
            .as_string()
            .or_else(|| value.as_f64().map(|number| number.to_string()))
            .unwrap_or_else(|| format!("{value:?}"));
        handle(&state, text);
    })
}
