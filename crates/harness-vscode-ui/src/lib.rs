//! VS Code webview UI, compiled to wasm32 (SPEC §11).
//!
//! Renders typed messages from the extension (`harness-vscode/src/protocol.rs`) into the transcript
//! and status line, and sends prompts, stop and permission answers back as typed messages.

mod view;

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use view::{Phase, Status};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    Document, Element, Event, HtmlElement, HtmlFormElement, HtmlInputElement, HtmlTextAreaElement,
    KeyboardEvent, MessageEvent,
};

#[wasm_bindgen]
extern "C" {
    #[derive(Clone)]
    type VsCodeApi;
    #[wasm_bindgen(js_name = acquireVsCodeApi)]
    fn acquire_vscode_api() -> VsCodeApi;
    #[wasm_bindgen(method, js_name = postMessage)]
    fn post_message(this: &VsCodeApi, message: &str);
}

struct Ui {
    document: Document,
    output: Element,
    status_line: Element,
    status: Status,
    /// Turn start and end in ms since the epoch, for elapsed time.
    started: f64,
    ended: Option<f64>,
    /// The assistant message being streamed and its markdown so far.
    message: Option<(Element, String)>,
    /// Tool call id to its summary and output elements.
    tools: HashMap<String, (Element, Element)>,
    /// Set by the extension's "ready" notice; cleared again by an exit or failure notice.
    ready: bool,
    send: Element,
    stop: Element,
    /// The file the active editor shows, and whether the user kept its chip.
    active: Option<String>,
    active_on: bool,
    /// Files picked with "Add file".
    added: Vec<String>,
    chips: Element,
    problems: HtmlInputElement,
}

/// A disabled control carries the attribute, which browsers and screen readers both honour.
fn set_enabled(element: &Element, enabled: bool) -> Result<(), JsValue> {
    if enabled {
        element.remove_attribute("disabled")
    } else {
        element.set_attribute("disabled", "")
    }
}

type Shared = Rc<RefCell<Ui>>;

fn by_id(document: &Document, id: &str) -> Result<Element, JsValue> {
    document
        .get_element_by_id(id)
        .ok_or_else(|| format!("no #{id}").into())
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;
    let output = by_id(&document, "output")?;
    let form: HtmlFormElement = by_id(&document, "prompt-form")?.dyn_into()?;
    let stop = by_id(&document, "stop")?;
    // The Send button has no id; it is the form's submit button.
    let send = form
        .query_selector("button[type=submit]")?
        .ok_or("no Send button")?;
    set_enabled(&send, false)?;
    set_enabled(&stop, false)?;
    let input: HtmlTextAreaElement = by_id(&document, "prompt")?.dyn_into()?;
    let problems: HtmlInputElement = by_id(&document, "problems")?.dyn_into()?;
    let chips = by_id(&document, "chips")?;
    let vscode = acquire_vscode_api();
    let ui: Shared = Rc::new(RefCell::new(Ui {
        status_line: by_id(&document, "status")?,
        document,
        output: output.clone(),
        status: Status {
            phase: Phase::Idle,
            context: None,
        },
        started: js_sys::Date::now(),
        ended: Some(js_sys::Date::now()),
        message: None,
        tools: HashMap::new(),
        ready: false,
        send,
        stop: stop.clone(),
        active: None,
        active_on: false,
        added: Vec::new(),
        chips: chips.clone(),
        problems: problems.clone(),
    }));

    let state = ui.clone();
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |event: MessageEvent| {
        let message = event
            .data()
            .as_string()
            .and_then(|text| serde_json::from_str(&text).ok());
        if let Some(message) = message {
            // Errors here mean a DOM call failed; nothing useful to do but keep the panel alive.
            let _ = state.borrow_mut().handle(&message);
        }
    });
    window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref())?;

    // Enter goes through the form's submit path, so the Send handler stays the only send logic.
    let submit_form = form.clone();
    let on_key = Closure::<dyn FnMut(KeyboardEvent)>::new(move |event: KeyboardEvent| {
        if view::enter_sends(&event.key(), event.shift_key(), event.is_composing()) {
            event.prevent_default();
            // A failed submit leaves the prompt in place, which is the right outcome.
            let _ = submit_form.request_submit();
        }
    });
    input.add_event_listener_with_callback("keydown", on_key.as_ref().unchecked_ref())?;
    on_key.forget();

    let (state, api) = (ui.clone(), vscode.clone());
    let on_submit = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        let text = input.value();
        // Before the session is ready a prompt would only be refused, so the text stays in the box.
        if !text.trim().is_empty() && state.borrow().ready {
            let ui = state.borrow();
            let message = json!({"type": "prompt", "text": text, "file": ui.attached_file(),
                "files": ui.added, "problems": ui.problems.checked()});
            drop(ui);
            api.post_message(&message.to_string());
            input.set_value("");
            let _ = state.borrow_mut().begin(&text);
        }
    });
    form.add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref())?;

    let api = vscode.clone();
    let on_stop = Closure::<dyn FnMut()>::new(move || {
        api.post_message(&json!({"type": "cancel"}).to_string());
    });
    stop.add_event_listener_with_callback("click", on_stop.as_ref().unchecked_ref())?;

    // One delegated listener answers every permission prompt; buttons carry their request and option.
    let api = vscode.clone();
    let on_click = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(button) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        let (Some(id), Some(option)) = (
            button.get_attribute("data-request"),
            button.get_attribute("data-option"),
        ) else {
            return;
        };
        // The attribute holds the request id as JSON, so number and string ids both round-trip.
        let Ok(id) = serde_json::from_str::<Value>(&id) else {
            return;
        };
        api.post_message(&json!({"type": "permission", "id": id, "option": option}).to_string());
        if let Ok(Some(prompt)) = button.closest("fieldset") {
            prompt.remove();
        }
    });
    output.add_event_listener_with_callback("click", on_click.as_ref().unchecked_ref())?;

    // Removing a chip drops the file from the next prompt; the active file's chip comes back on a new file.
    let state = ui.clone();
    let on_chip = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(button) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        let Ok(Some(chip)) = button.closest("[data-chip]") else {
            return;
        };
        let key = chip.get_attribute("data-chip").unwrap_or_default();
        let mut ui = state.borrow_mut();
        if key == "active" {
            ui.active_on = false;
        } else if let Some(index) = key
            .strip_prefix("added:")
            .and_then(|i| i.parse::<usize>().ok())
            && index < ui.added.len()
        {
            ui.added.remove(index);
        }
        let _ = ui.render_chips();
    });
    chips.add_event_listener_with_callback("click", on_chip.as_ref().unchecked_ref())?;
    on_chip.forget();

    let api = vscode.clone();
    let on_add = Closure::<dyn FnMut()>::new(move || {
        api.post_message(&json!({"type": "add_file"}).to_string());
    });
    by_id(&window.document().ok_or("no document")?, "add-file")?
        .add_event_listener_with_callback("click", on_add.as_ref().unchecked_ref())?;
    on_add.forget();

    let state = ui;
    let tick = Closure::<dyn FnMut()>::new(move || state.borrow().show_status());
    window.set_interval_with_callback_and_timeout_and_arguments_0(
        tick.as_ref().unchecked_ref(),
        1000,
    )?;
    // All listeners live as long as the page.
    on_message.forget();
    on_submit.forget();
    on_stop.forget();
    on_click.forget();
    tick.forget();

    vscode.post_message(&json!({"type": "ready"}).to_string());
    Ok(())
}

impl Ui {
    fn append(&self, tag: &str, class: &str, text: &str) -> Result<Element, JsValue> {
        let element = self.document.create_element(tag)?;
        element.set_class_name(class);
        element.set_text_content(Some(text));
        self.output.append_child(&element)?;
        Ok(element)
    }

    fn begin(&mut self, prompt: &str) -> Result<(), JsValue> {
        self.message = None;
        self.append("p", "user", prompt)?;
        self.started = js_sys::Date::now();
        self.ended = None;
        self.status.phase = Phase::ProcessingPrompt;
        self.output.set_attribute("aria-busy", "true")?;
        set_enabled(&self.stop, true)?;
        self.show_status();
        Ok(())
    }

    /// Leaves the busy state, so a dead or failed session never looks like it is still working.
    fn idle(&mut self) -> Result<(), JsValue> {
        self.message = None;
        self.status.phase = Phase::Idle;
        self.ended = Some(js_sys::Date::now());
        self.output.remove_attribute("aria-busy")?;
        set_enabled(&self.stop, false)
    }

    fn attached_file(&self) -> Option<String> {
        self.active.clone().filter(|_| self.active_on)
    }

    /// Redraws the chips for the active file and the picked files, each with a remove button.
    fn render_chips(&self) -> Result<(), JsValue> {
        self.chips.set_text_content(Some(""));
        let active = self.attached_file().map(|path| (path, "active".to_owned()));
        let picked = self
            .added
            .iter()
            .enumerate()
            .map(|(index, path)| (path.clone(), format!("added:{index}")));
        for (path, key) in active.into_iter().chain(picked) {
            let name = path
                .rsplit(['/', '\\'])
                .next()
                .unwrap_or(path.as_str())
                .to_owned();
            let chip = self.document.create_element("span")?;
            chip.set_class_name("chip");
            chip.set_attribute("title", &path)?;
            let label = self.document.create_element("span")?;
            label.set_text_content(Some(&name));
            let remove = self.document.create_element("button")?;
            remove.set_attribute("type", "button")?;
            remove.set_attribute("data-chip", &key)?;
            remove.set_attribute("aria-label", &format!("Remove {name}"))?;
            remove.set_text_content(Some("\u{00d7}"));
            chip.append_child(&label)?;
            chip.append_child(&remove)?;
            self.chips.append_child(&chip)?;
        }
        Ok(())
    }

    fn set_ready(&mut self, ready: bool) -> Result<(), JsValue> {
        self.ready = ready;
        set_enabled(&self.send, ready)?;
        if ready { Ok(()) } else { self.idle() }
    }

    fn show_status(&self) {
        let end = self.ended.unwrap_or_else(js_sys::Date::now);
        let elapsed = ((end - self.started) / 1000.0).max(0.0) as u64;
        self.status_line
            .set_text_content(Some(&self.status.line(elapsed)));
    }

    fn handle(&mut self, message: &Value) -> Result<(), JsValue> {
        let text = |key: &str| message[key].as_str().unwrap_or_default();
        match text("type") {
            "notice" => {
                // These texts come from `harness-vscode/src/lib.rs` and `protocol.rs`.
                let notice = text("text");
                if notice == "ready" {
                    self.set_ready(true)?;
                } else if notice.starts_with("harness acp")
                    || notice.starts_with("Harness could not start")
                {
                    self.set_ready(false)?;
                }
                self.append("p", "notice", notice)?;
            }
            "text" => {
                self.status.phase = Phase::Generating;
                if self.message.is_none() {
                    let element = self.append("div", "message", "")?;
                    self.message = Some((element, String::new()));
                }
                if let Some((element, markdown)) = &mut self.message {
                    markdown.push_str(text("text"));
                    // Safe only because `view::markdown` escapes raw HTML and drops unsafe links.
                    element.set_inner_html(&view::markdown(markdown));
                }
            }
            "active" => {
                let path = message["path"].as_str().map(str::to_owned);
                // A new active file brings its chip back; the same file keeps the user's choice.
                if path != self.active {
                    self.active_on = true;
                }
                self.active = path;
                self.render_chips()?;
            }
            "picked" => {
                for file in message["files"]
                    .as_array()
                    .map(Vec::as_slice)
                    .unwrap_or_default()
                {
                    let Some(file) = file.as_str() else { continue };
                    if !self.added.iter().any(|known| known == file) {
                        self.added.push(file.to_owned());
                    }
                }
                self.render_chips()?;
            }
            "tool" => self.tool(message)?,
            "permission" => self.permission(message)?,
            "usage" => {
                if let (Some(used), Some(size)) =
                    (message["used"].as_u64(), message["size"].as_u64())
                {
                    self.status.context = Some((used, size));
                }
            }
            "done" => {
                self.idle()?;
                self.append("p", "notice", &format!("Turn ended: {}", text("reason")))?;
                // A prompt left open after the turn ended can no longer be answered.
                let stale = self.output.query_selector_all("fieldset")?;
                for index in 0..stale.length() {
                    if let Some(node) = stale.item(index) {
                        node.dyn_into::<Element>()?.remove();
                    }
                }
            }
            _ => return Ok(()),
        }
        self.show_status();
        Ok(())
    }

    fn tool(&mut self, message: &Value) -> Result<(), JsValue> {
        self.message = None;
        let id = message["id"].as_str().unwrap_or_default().to_owned();
        let status = message["status"].as_str().unwrap_or("pending");
        self.status.phase = match status {
            "pending" | "in_progress" => Phase::RunningTool,
            _ => Phase::ProcessingPrompt,
        };
        if !self.tools.contains_key(&id) {
            // Collapsed by default: tool output is long and the summary carries the status.
            let details = self.append("details", "tool", "")?;
            let summary = self.document.create_element("summary")?;
            let pre = self.document.create_element("pre")?;
            details.append_child(&summary)?;
            details.append_child(&pre)?;
            summary.set_attribute("data-title", "tool")?;
            self.tools.insert(id.clone(), (summary, pre));
        }
        let Some((summary, pre)) = self.tools.get(&id) else {
            return Ok(());
        };
        if let Some(title) = message["title"].as_str() {
            summary.set_attribute("data-title", title)?;
        }
        let title = summary.get_attribute("data-title").unwrap_or_default();
        summary.set_text_content(Some(&format!("{title} ({status})")));
        summary.set_attribute("data-status", status)?;
        if let Some(output) = message["output"].as_str() {
            pre.set_text_content(Some(output));
        }
        Ok(())
    }

    fn permission(&mut self, message: &Value) -> Result<(), JsValue> {
        self.message = None;
        let id = message["id"].to_string();
        let title = message["title"].as_str().unwrap_or("a tool");
        let prompt = self.append("fieldset", "permission", "")?;
        let legend = self.document.create_element("legend")?;
        legend.set_text_content(Some(&format!("Allow {title}?")));
        prompt.append_child(&legend)?;
        let options = message["options"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (index, option) in options.iter().enumerate() {
            let button: HtmlElement = self.document.create_element("button")?.dyn_into()?;
            button.set_attribute("type", "button")?;
            button.set_attribute("data-request", &id)?;
            button.set_attribute("data-option", option["id"].as_str().unwrap_or_default())?;
            button.set_text_content(option["name"].as_str());
            prompt.append_child(&button)?;
            if index == 0 {
                // Keyboard users land on the prompt instead of hunting for it.
                button.focus()?;
            }
        }
        Ok(())
    }
}
