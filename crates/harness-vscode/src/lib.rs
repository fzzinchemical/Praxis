//! VS Code extension host side, compiled to wasm32 (SPEC §11).
//!
//! `harness.open` opens a chat panel. Once its wasm reports ready, `harness acp` is spawned in the
//! first workspace folder; prompts, stop and permission answers go to it, typed updates come back.

mod protocol;

use js_sys::{Array, Object, Promise, Reflect};
use protocol::{Client, Context, FromPanel, Lines, Problem, Problems, Reaction, Review, Selection};
use std::cell::RefCell;
use std::rc::Rc;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

const SHELL: &str = include_str!("../../../editors/vscode/webview.html");
const VIEW_COLUMN_ACTIVE: i32 = -1;
const VIEW_COLUMN_BESIDE: i32 = -2;
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
    fn execute_command(
        command: &str,
        left: &Uri,
        right: &Uri,
        title: &str,
        options: &Object,
    ) -> Promise;
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
    #[wasm_bindgen(thread_local_v2, js_name = window)]
    static WINDOW: JsValue;

    #[wasm_bindgen(js_namespace = window, js_name = onDidChangeActiveTextEditor)]
    fn on_did_change_active_text_editor(listener: &Closure<dyn FnMut(JsValue)>) -> Disposable;
    #[wasm_bindgen(js_namespace = window, js_name = showOpenDialog)]
    fn show_open_dialog(options: &Object) -> Promise;
    #[wasm_bindgen(js_namespace = languages, js_name = getDiagnostics)]
    fn get_diagnostics(uri: &Uri) -> Array;

    type Uri;
    #[wasm_bindgen(static_method_of = Uri, js_name = joinPath)]
    fn join_path(base: &Uri, path: &str) -> Uri;
    #[wasm_bindgen(static_method_of = Uri, js_name = file)]
    fn file_uri(path: &str) -> Uri;
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
    #[wasm_bindgen(method, js_name = getText)]
    fn get_text_in(this: &TextDocument, range: &JsValue) -> String;

    /// What a VS Code `on*` registration returns; `dispose` removes the listener.
    type Disposable;
    #[wasm_bindgen(method)]
    fn dispose(this: &Disposable);

    /// A promise from VS Code. Its callbacks are `once_into_js` functions, which free themselves.
    type Thenable;
    #[wasm_bindgen(method, js_name = then)]
    fn then_once(this: &Thenable, on_ok: &JsValue) -> Thenable;
    #[wasm_bindgen(method, js_name = catch)]
    fn catch_once(this: &Thenable, on_err: &JsValue) -> Thenable;

    type WebviewPanel;
    #[wasm_bindgen(method, getter)]
    fn webview(this: &WebviewPanel) -> Webview;
    #[wasm_bindgen(method, js_name = onDidDispose)]
    fn on_did_dispose(this: &WebviewPanel, callback: &JsValue);

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
    fn on_did_receive_message(this: &Webview, callback: &Closure<dyn FnMut(JsValue)>)
    -> Disposable;
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
    fn on_child(this: &ChildProcess, event: &str, callback: &JsValue);
    #[wasm_bindgen(method, js_name = removeListener)]
    fn off_child(this: &ChildProcess, event: &str, callback: &JsValue);
    #[wasm_bindgen(method)]
    fn kill(this: &ChildProcess) -> bool;

    type Writable;
    #[wasm_bindgen(method)]
    fn write(this: &Writable, text: &str) -> bool;

    type Emitter;
    #[wasm_bindgen(method, js_name = setEncoding)]
    fn set_encoding(this: &Emitter, encoding: &str);
    #[wasm_bindgen(method)]
    fn on(this: &Emitter, event: &str, callback: &JsValue);
    #[wasm_bindgen(method, js_name = removeListener)]
    fn off(this: &Emitter, event: &str, callback: &JsValue);
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
    /// The child's event listeners; they must be removed before their closures drop.
    listeners: Option<ChildListeners>,
    client: Option<Client>,
    lines: Lines,
    closed: bool,
    /// Webview and editor registrations, disposed when the panel closes.
    disposables: Vec<Disposable>,
    /// The closures behind `disposables`; they hold `Shared`, so they drop with the panel.
    closures: Vec<Closure<dyn FnMut(JsValue)>>,
}

/// The child's listeners. `close` is a once-closure, so it frees itself when it runs.
struct ChildListeners {
    stdout: Closure<dyn FnMut(JsValue)>,
    stderr: Closure<dyn FnMut(JsValue)>,
    error: Closure<dyn FnMut(JsValue)>,
    close: JsValue,
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

/// Unhooks the child's listeners, then kills it. Closures drop only after they are unhooked,
/// so a late event can't reach a freed closure.
fn detach(panel: &mut Panel) {
    let (Some(child), Some(listeners)) = (panel.child.take(), panel.listeners.take()) else {
        return;
    };
    child.stdout().off("data", listeners.stdout.as_ref());
    child.stderr().off("data", listeners.stderr.as_ref());
    child.off_child("error", listeners.error.as_ref());
    child.off_child("close", &listeners.close);
    child.kill();
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
    // Without it VS Code discards the webview when its tab is hidden, and the conversation with it.
    Reflect::set(&options, &"retainContextWhenHidden".into(), &true.into())?;
    let roots = Array::of1(root.unchecked_ref());
    Reflect::set(&options, &"localResourceRoots".into(), &roots)?;
    let panel = create_webview_panel("harness.chat", "Harness", VIEW_COLUMN_ACTIVE, &options)?;
    let webview = panel.webview();
    let shared: Shared = Rc::new(RefCell::new(Panel {
        webview: webview.clone(),
        output: output.clone(),
        child: None,
        listeners: None,
        client: None,
        lines: Lines::default(),
        closed: false,
        disposables: Vec::new(),
        closures: Vec::new(),
    }));

    let state = shared.clone();
    let on_message = Closure::<dyn FnMut(JsValue)>::new(move |message: JsValue| {
        match message.as_string().as_deref().and_then(FromPanel::parse) {
            Some(FromPanel::Ready) => {
                post_active(&state);
                start(&state);
            }
            Some(FromPanel::Prompt {
                text,
                file,
                files,
                problems,
            }) => send_prompt(&state, &text, file, files, problems),
            Some(FromPanel::AddFile) => add_files(&state),
            Some(FromPanel::Cancel) => {
                let mut panel = state.borrow_mut();
                let lines = panel.client.as_mut().map(Client::cancel);
                panel.send(lines.unwrap_or_default());
            }
            Some(FromPanel::Model(key)) => {
                let mut panel = state.borrow_mut();
                let line = panel
                    .client
                    .as_mut()
                    .and_then(|client| client.set_model(&key));
                panel.send(line);
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
    let on_editor = {
        let state = shared.clone();
        Closure::<dyn FnMut(JsValue)>::new(move |_editor: JsValue| post_active(&state))
    };
    let message_registration = webview.on_did_receive_message(&on_message);
    let editor_registration = on_did_change_active_text_editor(&on_editor);

    let state = shared.clone();
    let on_dispose = Closure::once_into_js(move || {
        let (disposables, closures) = {
            let mut panel = state.borrow_mut();
            panel.closed = true;
            detach(&mut panel);
            (
                std::mem::take(&mut panel.disposables),
                std::mem::take(&mut panel.closures),
            )
        };
        for registration in disposables {
            registration.dispose();
        }
        // Dropping the closures releases their clones of `state`, so the panel can free.
        drop(closures);
    });
    panel.on_did_dispose(&on_dispose);

    {
        let mut panel = shared.borrow_mut();
        panel.disposables = vec![message_registration, editor_registration];
        panel.closures = vec![on_message, on_editor];
    }

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
    child.stdout().on("data", on_stdout.as_ref());
    let on_stderr = listener(state, |state, chunk| state.borrow().output.append(&chunk));
    child.stderr().on("data", on_stderr.as_ref());
    // Node emits `error` for a missing binary; unhandled, it would crash the extension host.
    let on_error = listener(state, |state, error| {
        let panel = state.borrow();
        panel.output.append(&format!("harness acp: {error}\n"));
        panel
            .webview
            .notify(&format!("harness acp failed: {error}"));
    });
    child.on_child("error", on_error.as_ref());
    let on_close = {
        let state = state.clone();
        Closure::once_into_js(move |code: JsValue| exited(&state, &text_of(code)))
    };
    child.on_child("close", &on_close);

    let mut panel = state.borrow_mut();
    detach(&mut panel);
    panel.child = Some(child);
    panel.listeners = Some(ChildListeners {
        stdout: on_stdout,
        stderr: on_stderr,
        error: on_error,
        close: on_close,
    });
    panel.client = Some(client);
    panel.lines = Lines::default();
}

/// Tells the webview which file the active editor shows, so its chip follows the editor.
fn post_active(state: &Shared) {
    let panel = state.borrow();
    if !panel.closed {
        panel
            .webview
            .post_message(&protocol::active_file(active_path().as_deref()).to_string());
    }
}

/// Attaches the active file's selection and problems when the webview kept its chip.
fn send_prompt(
    state: &Shared,
    text: &str,
    file: Option<String>,
    files: Vec<String>,
    problems: bool,
) {
    let active = active_path();
    let selection = file.as_deref().and_then(active_selection);
    let problems = file
        .as_deref()
        .filter(|path| problems && active.as_deref() == Some(*path))
        .and_then(problems_of);
    let context = Context {
        files: file.into_iter().chain(files).collect(),
        selection,
        problems,
    };
    let mut panel = state.borrow_mut();
    let line = panel
        .client
        .as_mut()
        .and_then(|client| client.prompt(text, &context));
    match (line, &panel.child) {
        (Some(line), Some(child)) => {
            child.stdin().write(&format!("{line}\n"));
        }
        _ => panel.webview.notify("Harness is not ready yet."),
    }
}

/// Opens VS Code's file picker. Paths outside the workspace are refused, not attached.
fn add_files(state: &Shared) {
    let Some(cwd) = workspace_dir() else {
        return;
    };
    let options = Object::new();
    let set = |key: &str, value: JsValue| Reflect::set(&options, &key.into(), &value);
    let _ = set("canSelectMany", true.into());
    let _ = set("canSelectFolders", false.into());
    let _ = set("defaultUri", Uri::file_uri(&cwd).into());
    let (webview, output) = {
        let panel = state.borrow();
        (panel.webview.clone(), panel.output.clone())
    };
    let on_picked = Closure::once_into_js(move |picked: JsValue| {
        // The picker resolves to undefined when the user cancels.
        if picked.is_undefined() {
            return;
        }
        let (inside, outside): (Vec<String>, Vec<String>) = Array::from(&picked)
            .iter()
            .map(|uri| uri.unchecked_into::<Uri>().fs_path())
            .partition(|path| protocol::inside(&cwd, path));
        if !outside.is_empty() {
            webview.notify(&format!(
                "Not attached, outside the workspace: {}",
                outside.join(", ")
            ));
        }
        webview.post_message(&protocol::picked_files(&inside).to_string());
    });
    let failed = failure_logger(&output);
    let _ = show_open_dialog(&options)
        .unchecked_into::<Thenable>()
        .then_once(&on_picked)
        .catch_once(&failed);
}

/// The active editor, if it shows a file on disk.
fn active_editor() -> Option<JsValue> {
    let editor = WINDOW
        .with(|window| Reflect::get(window, &"activeTextEditor".into()))
        .ok()?;
    (!editor.is_null() && !editor.is_undefined()).then_some(editor)
}

fn property(value: &JsValue, key: &str) -> Option<JsValue> {
    Reflect::get(value, &key.into()).ok()
}

/// The path of the active editor's file, unsaved or not.
fn active_path() -> Option<String> {
    let editor = active_editor()?;
    editor_path(&editor)
}

fn editor_path(editor: &JsValue) -> Option<String> {
    let uri = property(&property(editor, "document")?, "uri")?;
    (property(&uri, "scheme")?.as_string()? == "file")
        .then(|| property(&uri, "fsPath")?.as_string())
        .flatten()
}

/// The active editor's non-empty selection, when it shows `path`.
fn active_selection(path: &str) -> Option<Selection> {
    let editor = active_editor()?;
    if editor_path(&editor)? != path {
        return None;
    }
    let range = property(&editor, "selection")?;
    if property(&range, "isEmpty")?.as_bool()? {
        return None;
    }
    let document: TextDocument = property(&editor, "document")?.unchecked_into();
    let line = |key: &str| Some(property(&property(&range, key)?, "line")?.as_f64()? as u64 + 1);
    Some(Selection {
        path: path.to_owned(),
        text: document.get_text_in(&range),
        start_line: line("start")?,
        end_line: line("end")?,
    })
}

/// The diagnostics VS Code holds for `path`, as the agent will read them.
fn problems_of(path: &str) -> Option<Problems> {
    let items = get_diagnostics(&Uri::file_uri(path))
        .iter()
        .filter_map(|diagnostic| {
            let start = property(&property(&diagnostic, "range")?, "start")?;
            Some(Problem {
                line: property(&start, "line")?.as_f64()? as u64 + 1,
                severity: protocol::severity(property(&diagnostic, "severity")?.as_f64()? as u64),
                message: property(&diagnostic, "message")?.as_string()?,
            })
        })
        .collect();
    Some(Problems {
        path: path.to_owned(),
        items,
    })
}

fn react(state: &Shared, reaction: Reaction) {
    let panel = state.borrow();
    if let Some(message) = reaction.show {
        panel.webview.post_message(&message.to_string());
    }
    if let Some(models) = reaction.models {
        panel.webview.post_message(&models.to_string());
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
    let on_sides = Closure::once_into_js(move |opened: JsValue| {
        let opened = Array::from(&opened);
        let left = opened.get(0).unchecked_into::<TextDocument>().uri();
        let right = opened.get(1).unchecked_into::<TextDocument>().uri();
        // Beside the chat and without focus: in the chat's own group the diff covers the
        // Allow/Reject card, and the turn waits on an answer nobody can see.
        let options = Object::new();
        let _ = Reflect::set(&options, &"viewColumn".into(), &VIEW_COLUMN_BESIDE.into());
        let _ = Reflect::set(&options, &"preserveFocus".into(), &true.into());
        let _ = execute_command("vscode.diff", &left, &right, &title, &options)
            .unchecked_into::<Thenable>()
            .catch_once(&failed_diff);
    });
    let _ = Promise::all(&sides)
        .unchecked_into::<Thenable>()
        .then_once(&on_sides)
        .catch_once(&failed_open);
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
fn failure_logger(output: &OutputChannel) -> JsValue {
    let output = output.clone();
    Closure::once_into_js(move |error: JsValue| {
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
        detach(&mut panel);
        panel.client = None;
        if panel.closed {
            return;
        }
        panel.webview.notify(&format!("harness acp exited: {code}"));
    }
    let message = format!("Harness exited ({code}). Its log is in the Harness output channel.");
    let state = state.clone();
    let on_choice = Closure::once_into_js(move |choice: JsValue| {
        if choice.as_string().as_deref() == Some(RESTART) && !state.borrow().closed {
            start(&state);
        }
    });
    let _ = show_error_with_action(&message, RESTART)
        .unchecked_into::<Thenable>()
        .then_once(&on_choice);
}

/// The text of a Node event argument: a string, a number, or its debug form.
fn text_of(value: JsValue) -> String {
    value
        .as_string()
        .or_else(|| value.as_f64().map(|number| number.to_string()))
        .unwrap_or_else(|| format!("{value:?}"))
}

/// Wraps `handle` as a Node event listener, turning its argument into text.
fn listener(state: &Shared, handle: fn(&Shared, String)) -> Closure<dyn FnMut(JsValue)> {
    let state = state.clone();
    Closure::new(move |value: JsValue| handle(&state, text_of(value)))
}
