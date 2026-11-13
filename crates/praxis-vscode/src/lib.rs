//! VS Code extension host side, compiled to wasm32 (SPEC §11).
//!
//! `praxis.open` opens a chat panel. Once its wasm reports ready, `praxis acp` is spawned in the
//! first workspace folder; prompts, stop and permission answers go to it, typed updates come back.

mod details;
mod plugins;
mod protocol;

use js_sys::{Array, Object, Promise, Reflect};
use plugins::{PluginItem, Verb, install_args, list_args, plugin_items, verb_args};
use protocol::{
    Client, Context, FromPanel, Lines, Problem, Problems, Reaction, Review, Selection, SessionItem,
    folder_args, folder_names, session_items,
};
use serde_json::Value;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

const SHELL: &str = include_str!("../../../editors/vscode/webview.html");
const VIEW_COLUMN_ACTIVE: i32 = -1;
const VIEW_COLUMN_BESIDE: i32 = -2;
const RESTART: &str = "Restart";
const CHAT_TITLE: &str = "Praxis";
const REVEAL: &str = "Show chat";

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
    fn register_command(
        id: &str,
        callback: &Closure<dyn FnMut(JsValue)>,
    ) -> Result<JsValue, JsValue>;

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
    #[wasm_bindgen(js_namespace = window, js_name = showInformationMessage)]
    fn show_information_with_action(message: &str, action: &str) -> Promise;

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
    // Same command as above, for commands that take one file Uri.
    #[wasm_bindgen(js_namespace = commands, js_name = executeCommand)]
    fn execute_uri_command(command: &str, uri: &Uri) -> Promise;
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
    #[wasm_bindgen(js_namespace = window, js_name = showWarningMessage)]
    fn show_warning_message(message: &str, options: &JsValue, button: &str) -> Promise;
    #[wasm_bindgen(js_namespace = window, js_name = showInputBox)]
    fn show_input_box(options: &JsValue) -> Promise;
    #[wasm_bindgen(js_namespace = window, js_name = createTreeView)]
    fn create_tree_view(id: &str, options: &JsValue) -> JsValue;
    #[wasm_bindgen(js_namespace = languages, js_name = getDiagnostics)]
    fn get_diagnostics(uri: &Uri) -> Array;

    #[derive(Clone)]
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
    /// True only for unsaved edits; VS Code's buffer otherwise differs from disk (BOM, EOLs).
    #[wasm_bindgen(method, getter, js_name = isDirty)]
    fn is_dirty(this: &TextDocument) -> bool;
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

    #[derive(Clone)]
    type WebviewPanel;
    #[wasm_bindgen(method, getter)]
    fn webview(this: &WebviewPanel) -> Webview;
    #[wasm_bindgen(method, js_name = onDidDispose)]
    fn on_did_dispose(this: &WebviewPanel, callback: &JsValue);
    #[wasm_bindgen(method)]
    fn dispose(this: &WebviewPanel);
    #[wasm_bindgen(method)]
    fn reveal(this: &WebviewPanel);

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

#[wasm_bindgen(module = "vscode")]
extern "C" {
    /// `vscode.EventEmitter`, which tells the sessions view to redraw.
    #[derive(Clone)]
    type EventEmitter;
    #[wasm_bindgen(constructor)]
    fn new() -> EventEmitter;
    #[wasm_bindgen(method, getter)]
    fn event(this: &EventEmitter) -> JsValue;
    #[wasm_bindgen(method)]
    fn fire(this: &EventEmitter, data: &JsValue);
}

#[wasm_bindgen(module = "fs")]
extern "C" {
    #[wasm_bindgen(catch, js_name = readFileSync)]
    fn read_file_sync(path: &str, encoding: &str) -> Result<JsValue, JsValue>;
}

#[wasm_bindgen(module = "child_process")]
extern "C" {
    #[derive(Clone)]
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

/// One panel and the `praxis acp` process behind it.
struct Panel {
    webview: Webview,
    /// The VS Code panel itself, disposed when its session is deleted.
    handle: WebviewPanel,
    output: OutputChannel,
    child: Option<ChildProcess>,
    /// The child's event listeners; they must be removed before their closures drop.
    listeners: Option<ChildListeners>,
    client: Option<Client>,
    lines: Lines,
    closed: bool,
    /// The session a sidebar click asked to open, loaded instead of a new one.
    resume: Option<String>,
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

thread_local! {
    /// Every chat panel opened, so deleting a session can close the panel showing it.
    static PANELS: RefCell<Vec<Weak<RefCell<Panel>>>> = const { RefCell::new(Vec::new()) };
}

/// Entry point VS Code calls. An `Err` is thrown, which fails activation visibly.
#[wasm_bindgen]
pub fn activate(context: ExtensionContext) -> Result<(), JsValue> {
    let root = context.extension_uri();
    let output = create_output_channel("Praxis");
    let panel = {
        let (root, output) = (root.clone(), output.clone());
        move |resume: Option<String>| {
            if let Err(error) = open_panel(&root, &output, resume) {
                show_error_message(&format!("Praxis: could not open the panel: {error:?}"));
            }
        }
    };
    create_sessions_view(&output);
    refresh_sessions();
    create_plugins_view(&output);
    refresh_plugins();
    command(&context, "praxis.enablePlugin", |item| {
        if let Some(name) = item_id(&item) {
            enable_plugin(name);
        }
    })?;
    command(&context, "praxis.disablePlugin", |item| {
        if let Some(name) = item_id(&item) {
            run_action(verb_args(Verb::Disable, &name));
        }
    })?;
    command(&context, "praxis.removePlugin", |item| {
        if let Some(name) = item_id(&item) {
            remove_plugin(name);
        }
    })?;
    command(&context, "praxis.showPlugin", |name| {
        if let Some(name) = name.as_string() {
            show_details(name);
        }
    })?;
    command(&context, "praxis.installPlugin", |_| install_plugin())?;
    command(&context, "praxis.refreshPlugins", |_| refresh_plugins())?;
    let open = panel.clone();
    command(&context, "praxis.open", move |_| open(None))?;
    let new_session = panel.clone();
    command(&context, "praxis.newSession", move |_| new_session(None))?;
    command(&context, "praxis.openSession", move |id| {
        panel(id.as_string())
    })?;
    command(&context, "praxis.deleteSession", |item| {
        if let Some(id) = item_id(&item) {
            delete_session(id);
        }
    })?;
    command(&context, "praxis.moveSessionToFolder", |item| {
        if let Some(id) = item_id(&item) {
            move_to_folder(id);
        }
    })?;
    command(&context, "praxis.refreshSessions", |_| refresh_sessions())
}

/// Registers a command whose handler receives the command's arguments, and keeps the handler alive.
fn command(
    context: &ExtensionContext,
    id: &str,
    handle: impl FnMut(JsValue) + 'static,
) -> Result<(), JsValue> {
    let callback = Closure::<dyn FnMut(JsValue)>::new(handle);
    let disposable = register_command(id, &callback)?;
    context.subscriptions().push(&disposable);
    // shortcut: closures leak until the host exits; fine for a handful of commands, revisit if they multiply.
    callback.forget();
    Ok(())
}

/// Boolean webview options for the chat. `retainContextWhenHidden` keeps the conversation when the
/// tab is hidden; `enableFindWidget` lets Ctrl+F search it.
fn chat_option_flags() -> [(&'static str, bool); 3] {
    [
        ("enableScripts", true),
        ("retainContextWhenHidden", true),
        ("enableFindWidget", true),
    ]
}

/// The notice for a turn that ended while VS Code was unfocused; `None` when the window has focus.
fn turn_notice(focused: bool, title: &str, reason: &str) -> Option<String> {
    (!focused).then(|| format!("{title}: turn ended ({reason})."))
}

/// Whether the VS Code window has focus. Unreadable counts as focused, so nothing is shown.
fn window_focused() -> bool {
    WINDOW
        .with(|window| property(window, "state"))
        .and_then(|state| property(&state, "focused"))
        .and_then(|focused| focused.as_bool())
        .unwrap_or(true)
}

/// Shows the turn notice; its button reveals the chat panel.
fn notify_turn_end(panel: WebviewPanel, text: &str) {
    let on_choice = Closure::once_into_js(move |choice: JsValue| {
        if choice.as_string().as_deref() == Some(REVEAL) {
            panel.reveal();
        }
    });
    let _ = show_information_with_action(text, REVEAL)
        .unchecked_into::<Thenable>()
        .then_once(&on_choice);
}

fn open_panel(root: &Uri, output: &OutputChannel, resume: Option<String>) -> Result<(), JsValue> {
    let options = Object::new();
    for (key, value) in chat_option_flags() {
        Reflect::set(&options, &key.into(), &value.into())?;
    }
    let roots = Array::of1(root.unchecked_ref());
    Reflect::set(&options, &"localResourceRoots".into(), &roots)?;
    let panel = create_webview_panel("praxis.chat", CHAT_TITLE, VIEW_COLUMN_ACTIVE, &options)?;
    let webview = panel.webview();
    let shared: Shared = Rc::new(RefCell::new(Panel {
        webview: webview.clone(),
        handle: panel.clone(),
        output: output.clone(),
        child: None,
        listeners: None,
        client: None,
        lines: Lines::default(),
        closed: false,
        disposables: Vec::new(),
        closures: Vec::new(),
        resume,
    }));

    track(&shared);
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
            Some(FromPanel::Dropped(uris)) => drop_files(&state, &uris),
            Some(FromPanel::OpenChange(path)) => open_change(&state, &path),
            Some(FromPanel::StopSubagent(id)) => {
                let panel = state.borrow();
                let line = panel
                    .client
                    .as_ref()
                    .and_then(|client| client.stop_subagent(&id));
                panel.send(line);
            }
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
            Some(FromPanel::PermissionMode(key)) => {
                let mut panel = state.borrow_mut();
                let line = panel
                    .client
                    .as_mut()
                    .and_then(|client| client.set_mode(&key));
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

    let script = webview.as_webview_uri(&Uri::join_path(root, "ui/praxis_vscode_ui.js"));
    let style = webview.as_webview_uri(&Uri::join_path(root, "ui/ui.css"));
    webview.set_html(
        &SHELL
            .replace("{{cspSource}}", &webview.csp_source())
            .replace("{{uiScript}}", &script.to_uri_string())
            .replace("{{uiStyle}}", &style.to_uri_string()),
    );
    Ok(())
}

/// The first workspace folder: `praxis acp` runs there and reads its project config.
fn workspace_dir() -> Option<String> {
    let folders = WORKSPACE.with(|workspace| Reflect::get(workspace, &"workspaceFolders".into()));
    let folder = Reflect::get(&folders.ok()?, &0.into()).ok()?;
    let uri = Reflect::get(&folder, &"uri".into()).ok()?;
    Reflect::get(&uri, &"fsPath".into()).ok()?.as_string()
}

/// `--config-override` pairs for the settings the user set. Unset ones keep config.toml's values.
fn config_overrides(config: &Configuration) -> Vec<String> {
    const KEYS: [(&str, &str); 6] = [
        ("defaultModel", "default_model"),
        ("permissions.mode", "permissions.mode"),
        ("lmstudio.baseUrl", "lmstudio.base_url"),
        ("agent.maxTurnRequests", "agent.max_turn_requests"),
        ("context.maskKeepTurns", "context.mask_keep_turns"),
        ("context.filterOutput", "context.filter_output"),
    ];
    let settings: Vec<(&str, Option<Value>)> = KEYS
        .iter()
        .map(|(vscode, toml)| (*toml, setting(config, vscode)))
        .collect();
    override_args(&settings)
}

fn setting(config: &Configuration, key: &str) -> Option<Value> {
    let value = config.get(key);
    if let Some(text) = value.as_string() {
        return (!text.is_empty()).then_some(Value::String(text));
    }
    if let Some(flag) = value.as_bool() {
        return Some(Value::Bool(flag));
    }
    // shortcut: fractional numbers are dropped; the settings schema's "integer" type rejects them in the UI.
    let number = value.as_f64()?;
    (number >= 0.0 && number.fract() == 0.0).then_some(Value::from(number as u64))
}

/// Renders each set value as `key=<JSON>`, which the CLI reads as a TOML literal (JSON strings are valid TOML).
fn override_args(settings: &[(&str, Option<Value>)]) -> Vec<String> {
    settings
        .iter()
        .flat_map(|(key, value)| {
            value
                .iter()
                .flat_map(move |v| ["--config-override".to_owned(), format!("{key}={v}")])
        })
        .collect()
}

fn start(state: &Shared) {
    let Some(cwd) = workspace_dir() else {
        state
            .borrow()
            .webview
            .notify("Open a folder to use Praxis.");
        return;
    };
    // `praxis.binaryPath` is machine-scoped in package.json, so a repository can't redirect it.
    let child = match spawn_acp(&cwd) {
        Ok(child) => child,
        Err(error) => {
            return state
                .borrow()
                .webview
                .notify(&format!("spawn failed: {error:?}"));
        }
    };
    let resume = state.borrow().resume.clone();
    let (client, init) = match &resume {
        Some(session) => Client::resume(&cwd, session),
        None => Client::start(&cwd),
    };
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
        panel.output.append(&format!("praxis acp: {error}\n"));
        panel.webview.notify(&format!("praxis acp failed: {error}"));
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
        _ => panel.webview.notify("Praxis is not ready yet."),
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
        let paths = Array::from(&picked)
            .iter()
            .map(|uri| uri.unchecked_into::<Uri>().fs_path())
            .collect();
        attach(&webview, &cwd, paths);
    });
    let failed = failure_logger(&output);
    let _ = show_open_dialog(&options)
        .unchecked_into::<Thenable>()
        .then_once(&on_picked)
        .catch_once(&failed);
}

/// Files dropped on the panel. The webview is untrusted, so the paths go through the same check as the picker.
fn drop_files(state: &Shared, uris: &[String]) {
    let Some(cwd) = workspace_dir() else {
        return;
    };
    let webview = state.borrow().webview.clone();
    attach(&webview, &cwd, protocol::dropped_paths(uris));
}

/// Opens a subagent's changed file as VS Code's Git diff, or as the file when git can't show it
/// (no repository). Paths outside the workspace are refused.
fn open_change(state: &Shared, path: &str) {
    let Some(cwd) = workspace_dir() else {
        return;
    };
    let Some(path) = protocol::change_target(&cwd, path) else {
        state
            .borrow()
            .webview
            .notify(&format!("Not opened, outside the workspace: {path}"));
        return;
    };
    let uri = Uri::file_uri(&path);
    let fallback = {
        let uri = uri.clone();
        Closure::once_into_js(move |_: JsValue| {
            let _ = execute_uri_command("vscode.open", &uri);
        })
    };
    let _ = execute_uri_command("git.openChange", &uri)
        .unchecked_into::<Thenable>()
        .catch_once(&fallback);
}

/// Posts the paths inside the workspace as picked files and says which ones were refused.
fn attach(webview: &Webview, cwd: &str, paths: Vec<String>) {
    let (inside, outside): (Vec<String>, Vec<String>) = paths
        .into_iter()
        .partition(|path| protocol::inside(cwd, path));
    if !outside.is_empty() {
        webview.notify(&format!(
            "Not attached, outside the workspace: {}",
            outside.join(", ")
        ));
    }
    if !inside.is_empty() {
        webview.post_message(&protocol::picked_files(&inside).to_string());
    }
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

fn react(state: &Shared, mut reaction: Reaction) {
    if let Some(message) = reaction
        .show
        .as_mut()
        .filter(|message| message["type"] == "done")
    {
        let limit = setting(&get_configuration("praxis"), "agent.maxTurnRequests")
            .and_then(|value| value.as_u64());
        let reason = message["reason"].as_str().unwrap_or_default();
        message["reason"] = Value::String(protocol::done_reason(reason, limit));
    }
    if let Some(message) = reaction
        .show
        .as_ref()
        .filter(|message| message["type"] == "done")
    {
        refresh_sessions();
        let reason = message["reason"].as_str().unwrap_or_default();
        if let Some(text) = turn_notice(window_focused(), CHAT_TITLE, reason) {
            notify_turn_end(state.borrow().handle.clone(), &text);
        }
    }
    let panel = state.borrow();
    if let Some(message) = reaction.show {
        panel.webview.post_message(&message.to_string());
    }
    if let Some(models) = reaction.models {
        panel.webview.post_message(&models.to_string());
    }
    if let Some(modes) = reaction.modes {
        panel.webview.post_message(&modes.to_string());
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

/// Picks the text for `fs/read_text_file`: the buffer only when it has unsaved edits.
fn choose_text(
    dirty: bool,
    buffer: impl FnOnce() -> Option<String>,
    disk: impl FnOnce() -> Option<String>,
) -> Option<String> {
    if dirty { buffer() } else { disk() }
}

/// The text of an open document at `path` when it has unsaved edits, else the file on disk.
fn editor_text(path: &str) -> Option<String> {
    let documents = WORKSPACE.with(|workspace| Reflect::get(workspace, &"textDocuments".into()));
    let documents: Array = documents.ok()?.unchecked_into();
    if let Some(document) = documents
        .iter()
        .map(|value| value.unchecked_into::<TextDocument>())
        .find(|document| document.uri().fs_path() == path)
    {
        return choose_text(
            document.is_dirty(),
            || Some(document.get_text()),
            || read_disk(path),
        );
    }
    read_disk(path)
}

/// The file on disk as text.
// shortcut: reads the whole file into memory with no size cap; cap it if large files show up.
fn read_disk(path: &str) -> Option<String> {
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
        panel.webview.notify(&format!("praxis acp exited: {code}"));
    }
    let message = format!("Praxis exited ({code}). Its log is in the Praxis output channel.");
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

/// `praxis acp` in `cwd`. `praxis.binaryPath` is machine-scoped in package.json, so a repository
/// can't redirect it.
fn spawn_acp(cwd: &str) -> Result<ChildProcess, JsValue> {
    let options = Object::new();
    Reflect::set(&options, &"cwd".into(), &cwd.into())?;
    let args = Array::new();
    for arg in config_overrides(&get_configuration("praxis")) {
        args.push(&arg.into());
    }
    args.push(&"acp".into());
    spawn(&binary_path(), &args, &options)
}

/// `praxis.binaryPath`, the CLI that the sessions and plugins views shell out to.
fn binary_path() -> String {
    get_configuration("praxis")
        .get("binaryPath")
        .as_string()
        .unwrap_or_else(|| "praxis".to_owned())
}

/// The sessions view, listed by the CLI like the Plugins view.
struct Sidebar {
    emitter: EventEmitter,
    output: OutputChannel,
    items: Vec<SessionItem>,
    /// Why the last listing failed, shown as the view's first item until a listing succeeds.
    problem: Option<String>,
}

thread_local! {
    static SIDEBAR: RefCell<Option<Sidebar>> = const { RefCell::new(None) };
}

/// Registers the sessions view. Its provider is a plain object, which VS Code accepts in place of a class.
fn create_sessions_view(output: &OutputChannel) {
    let emitter = EventEmitter::new();
    let provider = node(&[
        ("onDidChangeTreeData", emitter.event()),
        ("getTreeItem", callback(|item| item)),
        ("getChildren", callback(|element| children(&element).into())),
    ]);
    // The view lives as long as the host; the returned handle needs no keeping.
    let _ = create_tree_view("praxis.sessions", &node(&[("treeDataProvider", provider)]));
    SIDEBAR.with(|sidebar| {
        *sidebar.borrow_mut() = Some(Sidebar {
            emitter,
            output: output.clone(),
            items: Vec::new(),
            problem: None,
        });
    });
}

/// A closure VS Code calls back into. shortcut: provider closures live until the host exits; the view is created once.
fn callback(handle: impl FnMut(JsValue) -> JsValue + 'static) -> JsValue {
    Closure::<dyn FnMut(JsValue) -> JsValue>::new(handle).into_js_value()
}

/// A plain object with the given properties. A fresh object accepts any property, so the sets can't fail.
fn node(fields: &[(&str, JsValue)]) -> JsValue {
    let object = Object::new();
    for (key, value) in fields {
        let _ = Reflect::set(&object, &JsValue::from_str(key), value);
    }
    object.into()
}

/// The children of the view root, or of the folder `element` names. A folder holds the sessions filed in it.
fn children(element: &JsValue) -> Array {
    let nodes = match item_id(element)
        .as_deref()
        .and_then(|id| id.strip_prefix(FOLDER_PREFIX))
    {
        Some(folder) => folder_children(folder),
        None => root_children(),
    };
    let array = Array::new();
    for node in &nodes {
        array.push(node);
    }
    array
}

/// Prefixes a folder tree item's id, so `getChildren` can tell a folder from a session.
const FOLDER_PREFIX: &str = "folder:";

/// The root items: a failed listing first, then the folders and the sessions in no folder, or a line
/// saying there are none.
fn root_children() -> Vec<JsValue> {
    let (problem, mut nodes): (Option<String>, Vec<JsValue>) = SIDEBAR.with(|sidebar| {
        sidebar
            .borrow()
            .as_ref()
            .map(|sidebar| {
                let mut nodes: Vec<JsValue> = folder_names(&sidebar.items)
                    .iter()
                    .map(|name| folder_node(name))
                    .collect();
                nodes.extend(
                    sidebar
                        .items
                        .iter()
                        .filter(|item| item.folder.is_none())
                        .map(session_node),
                );
                (sidebar.problem.clone(), nodes)
            })
            .unwrap_or_default()
    });
    if let Some(problem) = problem {
        nodes.insert(
            0,
            node(&[
                (
                    "label",
                    JsValue::from_str(&format!("Could not list sessions: {problem}")),
                ),
                (
                    "tooltip",
                    JsValue::from_str("Details are in the Praxis output channel"),
                ),
            ]),
        );
    } else if nodes.is_empty() {
        nodes.push(node(&[(
            "label",
            JsValue::from_str("No sessions in this workspace yet"),
        )]));
    }
    nodes
}

/// The sessions filed in `folder`, newest first as the listing gives them.
fn folder_children(folder: &str) -> Vec<JsValue> {
    SIDEBAR.with(|sidebar| {
        sidebar
            .borrow()
            .as_ref()
            .map(|sidebar| {
                sidebar
                    .items
                    .iter()
                    .filter(|item| item.folder.as_deref() == Some(folder))
                    .map(session_node)
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// A folder as an expanded tree item, so the sessions in it show at once.
fn folder_node(name: &str) -> JsValue {
    node(&[
        ("id", JsValue::from_str(&format!("{FOLDER_PREFIX}{name}"))),
        ("label", JsValue::from_str(name)),
        ("contextValue", JsValue::from_str("folder")),
        // vscode.TreeItemCollapsibleState.Expanded
        ("collapsibleState", JsValue::from_f64(2.0)),
    ])
}

/// Asks for a folder name, then moves the session through the CLI. An empty name removes it from its
/// folder, and cancelling the box does nothing.
fn move_to_folder(id: String) {
    let options = node(&[
        (
            "prompt",
            JsValue::from_str("Folder name. Leave empty to take the session out of its folder."),
        ),
        ("placeHolder", JsValue::from_str("Folder")),
    ]);
    let on_name = Closure::once_into_js(move |name: JsValue| {
        let Some(args) = name.as_string().and_then(|name| folder_args(&id, &name)) else {
            return;
        };
        run_cli(args, None, |result| {
            if let Err(error) = result {
                sessions_log(&format!("sessions: move failed: {error}\n"));
            }
            refresh_sessions();
        });
    });
    let _ = show_input_box(&options)
        .unchecked_into::<Thenable>()
        .then_once(&on_name);
}

/// A session as a tree item whose click runs `praxis.openSession` with its id.
fn session_node(item: &SessionItem) -> JsValue {
    let command = node(&[
        ("command", JsValue::from_str("praxis.openSession")),
        ("title", JsValue::from_str("Open session")),
        ("arguments", Array::of1(&JsValue::from_str(&item.id)).into()),
    ]);
    node(&[
        ("id", JsValue::from_str(&item.id)),
        ("label", JsValue::from_str(&item.label)),
        ("contextValue", JsValue::from_str("session")),
        ("command", command),
    ])
}

/// Asks, then closes the panel showing the session (its agent stops, so the log is free), removes
/// the log through the CLI and redraws the list. Only ids in the session format reach the CLI.
fn delete_session(id: String) {
    let Some(args) = protocol::delete_args(&id) else {
        return sessions_log("refusing to delete a session with an unexpected id\n");
    };
    confirm(
        "Delete this session? Its log is removed and cannot be undone.",
        "Delete",
        move || {
            close_panels_of(&id);
            run_cli(args, None, |result| {
                if let Err(error) = result {
                    sessions_log(&format!("sessions: delete failed: {error}\n"));
                }
                refresh_sessions();
            });
        },
    );
}

/// Remembers a panel so deleting its session can find it. Dead entries are dropped here.
fn track(state: &Shared) {
    PANELS.with(|panels| {
        let mut panels = panels.borrow_mut();
        panels.retain(|panel| panel.strong_count() > 0);
        panels.push(Rc::downgrade(state));
    });
}

/// Disposes every open panel whose session is `id`. Their dispose handler stops the agent.
fn close_panels_of(id: &str) {
    let handles: Vec<WebviewPanel> = PANELS.with(|panels| {
        panels
            .borrow()
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|state| panel_session(&state.borrow()).as_deref() == Some(id))
            .map(|state| state.borrow().handle.clone())
            .collect()
    });
    // Disposing fires the dispose handler, which borrows the panel, so no borrow may be held here.
    for handle in handles {
        handle.dispose();
    }
}

/// The session a panel shows: the one it resumed, or the one the agent created or loaded.
fn panel_session(panel: &Panel) -> Option<String> {
    let loaded = panel.client.as_ref().and_then(Client::session_id);
    loaded.or(panel.resume.as_deref()).map(str::to_owned)
}

fn sessions_log(text: &str) {
    SIDEBAR.with(|sidebar| {
        if let Some(sidebar) = sidebar.borrow().as_ref() {
            sidebar.output.append(text);
        }
    });
}

/// Replaces the view's sessions and asks VS Code to redraw them.
fn publish(items: Vec<SessionItem>) {
    redraw(|sidebar| {
        sidebar.items = items;
        sidebar.problem = None;
    });
}

/// Logs a failed listing and shows it in the view, keeping the last known sessions.
fn fail(problem: String) {
    redraw(|sidebar| {
        sidebar.output.append(&format!("sessions: {problem}\n"));
        sidebar.problem = Some(problem);
    });
}

fn redraw(change: impl FnOnce(&mut Sidebar)) {
    let emitter = SIDEBAR.with(|sidebar| {
        sidebar.borrow_mut().as_mut().map(|sidebar| {
            change(sidebar);
            sidebar.emitter.clone()
        })
    });
    // Fired after the borrow ends: VS Code may call `getChildren` synchronously.
    if let Some(emitter) = emitter {
        emitter.fire(&JsValue::UNDEFINED);
    }
}

/// Re-reads this workspace's sessions from `praxis sessions --json` and redraws the view.
fn refresh_sessions() {
    let Some(cwd) = workspace_dir() else {
        return publish(Vec::new());
    };
    let args = vec!["sessions".to_owned(), "--json".to_owned()];
    run_cli(args, Some(&cwd), |result| {
        match result.and_then(|json| session_items(&json)) {
            Ok(items) => publish(items),
            Err(error) => fail(error),
        }
    });
}

/// The Plugins view: the last `plugin list --json`, redrawn after each action.
struct Plugins {
    emitter: EventEmitter,
    output: OutputChannel,
    items: Vec<PluginItem>,
}

thread_local! {
    static PLUGINS: RefCell<Option<Plugins>> = const { RefCell::new(None) };
    /// The open plugin details tabs, by plugin name; a tab removes itself when VS Code disposes it.
    static DETAILS: RefCell<std::collections::BTreeMap<String, WebviewPanel>> =
        const { RefCell::new(std::collections::BTreeMap::new()) };
}

/// Registers the Plugins view, same shape as the sessions view.
fn create_plugins_view(output: &OutputChannel) {
    let emitter = EventEmitter::new();
    let provider = node(&[
        ("onDidChangeTreeData", emitter.event()),
        ("getTreeItem", callback(|item| item)),
        ("getChildren", callback(|_| plugin_children().into())),
    ]);
    // The view lives as long as the host; the returned handle needs no keeping.
    let _ = create_tree_view("praxis.plugins", &node(&[("treeDataProvider", provider)]));
    PLUGINS.with(|plugins| {
        *plugins.borrow_mut() = Some(Plugins {
            emitter,
            output: output.clone(),
            items: Vec::new(),
        });
    });
}

fn plugin_children() -> Array {
    let mut nodes: Vec<JsValue> = PLUGINS.with(|plugins| {
        plugins
            .borrow()
            .as_ref()
            .map(|plugins| plugins.items.iter().map(plugin_node).collect())
            .unwrap_or_default()
    });
    if nodes.is_empty() {
        nodes.push(node(&[(
            "label",
            JsValue::from_str("No plugins installed"),
        )]));
    }
    let array = Array::new();
    for node in &nodes {
        array.push(node);
    }
    array
}

/// A plugin as a tree item. Its id is the plugin name, which the context-menu commands read back.
/// The context value picks the menu: enable only for disabled plugins, disable only for enabled ones.
fn plugin_node(item: &PluginItem) -> JsValue {
    // A broken plugin's context starts with `plugin-` so the Remove menu still matches it.
    let context = match (&item.error, item.enabled) {
        (Some(_), _) => "plugin-broken",
        (None, true) => "plugin-enabled",
        (None, false) => "plugin-disabled",
    };
    node(&[
        ("id", JsValue::from_str(&item.name)),
        ("label", JsValue::from_str(&item.name)),
        ("description", JsValue::from_str(&item.description())),
        ("contextValue", JsValue::from_str(context)),
        // A click opens the details tab; the name is the argument, since the item is not passed back.
        (
            "command",
            node(&[
                ("command", JsValue::from_str("praxis.showPlugin")),
                (
                    "arguments",
                    Array::of1(&JsValue::from_str(&item.name)).into(),
                ),
            ]),
        ),
    ])
}

/// Opens the plugin's details tab, or reveals the one already open for it. Each tab is one plugin.
fn show_details(name: String) {
    if let Some(panel) = DETAILS.with(|panels| panels.borrow().get(&name).cloned()) {
        panel.reveal();
        return;
    }
    let page = PLUGINS.with(|plugins| {
        plugins
            .borrow()
            .as_ref()
            .and_then(|plugins| plugins.items.iter().find(|item| item.name == name))
            .map(details::details_page)
    });
    let Some(page) = page else {
        return plugin_log(&format!("plugins: {name} is not listed\n"));
    };
    if let Err(error) = open_details(&name, &page) {
        plugin_log(&format!("plugins: details failed: {}\n", text_of(error)));
    }
}

/// Creates the read-only details tab and forgets it when VS Code closes it.
fn open_details(name: &str, page: &str) -> Result<(), JsValue> {
    let panel = create_webview_panel(
        "praxis.plugin",
        &format!("{name} (plugin)"),
        VIEW_COLUMN_ACTIVE,
        &Object::new(),
    )?;
    panel.webview().set_html(page);
    let key = name.to_owned();
    let on_dispose = Closure::once_into_js(move || {
        DETAILS.with(|panels| {
            panels.borrow_mut().remove(&key);
        });
    });
    panel.on_did_dispose(&on_dispose);
    DETAILS.with(|panels| panels.borrow_mut().insert(name.to_owned(), panel));
    Ok(())
}

fn publish_plugins(items: Vec<PluginItem>) {
    let emitter = PLUGINS.with(|plugins| {
        plugins.borrow_mut().as_mut().map(|plugins| {
            plugins.items = items;
            plugins.emitter.clone()
        })
    });
    // Fired after the borrow ends: VS Code may call `getChildren` synchronously.
    if let Some(emitter) = emitter {
        emitter.fire(&JsValue::UNDEFINED);
    }
}

fn plugin_log(text: &str) {
    PLUGINS.with(|plugins| {
        if let Some(plugins) = plugins.borrow().as_ref() {
            plugins.output.append(text);
        }
    });
}

/// The tree item a click or context-menu command was run on, by its `id`. From the palette the argument is absent.
fn item_id(item: &JsValue) -> Option<String> {
    Reflect::get(item, &"id".into()).ok()?.as_string()
}

/// Runs `praxis <args>` in `cwd` (the extension host's by default) and hands its result to `then`
/// once it exits: stdout on exit 0, stderr otherwise.
fn run_cli(
    args: Vec<String>,
    cwd: Option<&str>,
    then: impl FnOnce(Result<String, String>) + 'static,
) {
    let argv = Array::new();
    for arg in &args {
        argv.push(&arg.as_str().into());
    }
    let options: Object = match cwd {
        Some(cwd) => node(&[("cwd", JsValue::from_str(cwd))]).unchecked_into(),
        None => Object::new(),
    };
    let child = match spawn(&binary_path(), &argv, &options) {
        Ok(child) => child,
        Err(error) => return then(Err(format!("spawn failed: {error:?}"))),
    };
    child.stdout().set_encoding("utf8");
    child.stderr().set_encoding("utf8");
    let stdout = Rc::new(RefCell::new(String::new()));
    let stderr = Rc::new(RefCell::new(String::new()));
    let (out_sink, err_sink) = (stdout.clone(), stderr.clone());
    let on_stdout = Closure::<dyn FnMut(JsValue)>::new(move |chunk: JsValue| {
        out_sink.borrow_mut().push_str(&text_of(chunk));
    });
    let on_stderr = Closure::<dyn FnMut(JsValue)>::new(move |chunk: JsValue| {
        err_sink.borrow_mut().push_str(&text_of(chunk));
    });
    child.stdout().on("data", on_stdout.as_ref());
    child.stderr().on("data", on_stderr.as_ref());
    let on_error = Closure::<dyn FnMut(JsValue)>::new(|error: JsValue| {
        plugin_log(&format!("plugins: praxis: {}\n", text_of(error)));
    });
    child.on_child("error", on_error.as_ref());
    let on_close = Closure::once_into_js(move |code: JsValue| {
        let result = if code.as_f64() == Some(0.0) {
            Ok(stdout.borrow().clone())
        } else {
            Err(stderr.borrow().clone())
        };
        then(result);
    });
    child.on_child("close", &on_close);
    // shortcut: each run leaks its listeners until the host exits; plugin actions are rare, so the leak stays small.
    on_stdout.forget();
    on_stderr.forget();
    on_error.forget();
}

/// Re-reads the plugin list from the CLI and redraws the view.
fn refresh_plugins() {
    run_cli(list_args(), None, |result| {
        match result.and_then(|json| plugin_items(&json)) {
            Ok(items) => publish_plugins(items),
            Err(error) => plugin_log(&format!("plugins: list failed: {error}\n")),
        }
    });
}

/// Runs a CLI action, then refreshes the view whatever its outcome.
fn run_action(args: Vec<String>) {
    run_cli(args, None, |result| {
        if let Err(error) = result {
            plugin_log(&format!("plugins: {error}\n"));
        }
        refresh_plugins();
    });
}

/// Shows a modal with one button and runs `on_confirm` only if that button is picked.
fn confirm(message: &str, button: &'static str, on_confirm: impl FnOnce() + 'static) {
    let options = node(&[("modal", JsValue::TRUE)]);
    let on_choice = Closure::once_into_js(move |choice: JsValue| {
        if choice.as_string().as_deref() == Some(button) {
            on_confirm();
        }
    });
    let _ = show_warning_message(message, &options, button)
        .unchecked_into::<Thenable>()
        .then_once(&on_choice);
}

/// Shows what the plugin can run and enables it only if the user confirms.
fn enable_plugin(name: String) {
    let prompt = PLUGINS.with(|plugins| {
        plugins.borrow().as_ref().and_then(|plugins| {
            plugins
                .items
                .iter()
                .find(|item| item.name == name)
                .map(PluginItem::enable_prompt)
        })
    });
    match prompt {
        Some(prompt) => confirm(&prompt, "Enable", move || {
            run_action(verb_args(Verb::Enable, &name));
        }),
        None => plugin_log(&format!("plugins: {name} is not listed\n")),
    }
}

fn remove_plugin(name: String) {
    let message = format!("Remove {name}? Its data is kept.");
    confirm(&message, "Remove", move || {
        run_action(verb_args(Verb::Remove, &name));
    });
}

/// Asks for a path or git URL and installs it disabled. Cancel or blank input does nothing.
fn install_plugin() {
    let options = node(&[(
        "prompt",
        JsValue::from_str("Path to a plugin, or a git URL"),
    )]);
    let on_input = Closure::once_into_js(move |input: JsValue| {
        let source = input.as_string().map(|text| text.trim().to_owned());
        if let Some(source) = source.filter(|text| !text.is_empty()) {
            run_action(install_args(&source));
        }
    });
    let _ = show_input_box(&options)
        .unchecked_into::<Thenable>()
        .then_once(&on_input);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_only_explicitly_set_values_as_toml_literals() {
        let args = override_args(&[
            ("default_model", Some(Value::String("qwen3.5-4b".into()))),
            ("lmstudio.base_url", None),
            ("agent.max_turn_requests", Some(Value::from(40_u64))),
            ("context.filter_output", Some(Value::Bool(false))),
        ]);
        assert_eq!(
            args,
            [
                "--config-override",
                r#"default_model="qwen3.5-4b""#,
                "--config-override",
                "agent.max_turn_requests=40",
                "--config-override",
                "context.filter_output=false",
            ]
        );
    }

    #[test]
    fn no_settings_means_no_arguments() {
        assert!(override_args(&[("default_model", None)]).is_empty());
    }

    #[test]
    fn a_dirty_document_answers_with_its_buffer() {
        let text = choose_text(true, || Some("buffer".into()), || Some("disk".into()));
        assert_eq!(text.as_deref(), Some("buffer"));
    }

    #[test]
    fn a_clean_document_answers_with_the_disk_not_the_buffer() {
        // VS Code's buffer drops a BOM and rewrites mixed EOLs; only dirty text counts as unsaved.
        let text = choose_text(false, || Some("normalized".into()), || Some("disk".into()));
        assert_eq!(text.as_deref(), Some("disk"));
    }

    #[test]
    fn notifies_when_the_window_is_unfocused() {
        let text = turn_notice(false, "Praxis", "end_turn");
        assert!(text.is_some_and(|text| text.contains("Praxis") && text.contains("end_turn")));
    }

    #[test]
    fn silent_when_the_window_is_focused() {
        assert_eq!(turn_notice(true, "Praxis", "end_turn"), None);
    }

    #[test]
    fn chat_panel_enables_the_find_widget() {
        assert!(chat_option_flags().contains(&("enableFindWidget", true)));
    }
}
