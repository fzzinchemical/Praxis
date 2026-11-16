//! VS Code webview UI, compiled to wasm32 (SPEC §11).
//!
//! Renders typed messages from the extension (`praxis-vscode/src/protocol.rs`) into the transcript
//! and sends prompts, stop and permission answers back as typed messages.

mod queue;
mod view;

use serde_json::{Value, json};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use view::{Phase, subagent_summary};
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{
    Document, DragEvent, Element, Event, HtmlElement, HtmlFormElement, HtmlInputElement,
    HtmlOptionElement, HtmlSelectElement, HtmlTextAreaElement, KeyboardEvent, MediaQueryList,
    MessageEvent,
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

/// One tool call's collapsible block. An `Agent` call also holds its subagent's calls nested inside.
struct Tool {
    details: Element,
    summary: Element,
    pre: Element,
    title: String,
    status: String,
    /// The subagent's type, set only on an `Agent` call.
    kind: Option<String>,
    /// The subagent's name in the panel, given once when its type first arrives.
    name: Option<&'static str>,
    /// Calls nested under this one.
    calls: usize,
    /// The subagent's (prompt tokens, context length), once it has reported them.
    usage: Option<(u32, u32)>,
    /// The file a `Write` or `Edit` call changes.
    file: Option<String>,
    /// The `Agent` call this one runs under, and the calls nested under this one in arrival order.
    parent: Option<String>,
    children: Vec<String>,
    /// The changed-files list an `Agent` call shows, inside its block.
    files: Option<Element>,
    /// The Stop button an `Agent` call shows while its subagent runs.
    stop: Option<Element>,
}

/// Sets a block's summary as text, never HTML: titles and types come from the model.
fn show_summary(tool: &Tool) -> Result<(), JsValue> {
    let line = match &tool.kind {
        Some(kind) => subagent_summary(
            tool.name.unwrap_or("Subagent"),
            kind,
            tool.calls,
            &tool.status,
            tool.usage,
        ),
        None => format!("{} ({})", tool.title, tool.status),
    };
    tool.summary.set_text_content(Some(&line));
    tool.summary.set_attribute("data-title", &tool.title)?;
    tool.summary.set_attribute("data-status", &tool.status)?;
    // Only a running subagent can be stopped.
    if let Some(stop) = &tool.stop {
        match tool.status.as_str() {
            "pending" | "in_progress" => stop.remove_attribute("hidden")?,
            _ => stop.set_attribute("hidden", "")?,
        }
    }
    Ok(())
}

struct Ui {
    document: Document,
    output: Element,
    phase: Phase,
    /// Turn start and end in ms since the epoch, for elapsed time.
    started: f64,
    ended: Option<f64>,
    /// The assistant message being streamed and its markdown so far.
    message: Option<(Element, String)>,
    /// Tool call id to its block.
    tools: HashMap<String, Tool>,
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
    /// The model picker; enabled only while the session is idle and has models.
    model: HtmlSelectElement,
    /// The permission mode picker, enabled under the same rule as the model picker.
    mode: HtmlSelectElement,
    /// The gear beside the model picker, its popover and the popover's field list.
    gear: HtmlElement,
    profile_fields: Element,
    input: HtmlTextAreaElement,
    vscode: VsCodeApi,
    /// Prompts typed during a turn, and the panel that shows them above the input.
    queue: queue::Queue,
    queue_box: Element,
    queue_status: Element,
    queue_list: Element,
    /// User turns shown so far, so each bubble's "Rewind to here" knows its turn number.
    turns: usize,
    /// Subagents this panel has named so far, for the next name in the list.
    subagents: usize,
    /// Commands the agent announced for this session, and the autocomplete list over them.
    commands: Vec<view::Command>,
    menu: Element,
    /// Panel beside the list holding the description of the hovered or highlighted command.
    command_doc: Element,
    /// Whether the list is open (closed by Escape, reopened by typing) and its highlighted row.
    menu_open: bool,
    selected: usize,
}

fn parse_commands(message: &Value) -> Vec<view::Command> {
    message["commands"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some(view::Command {
                        name: item["name"].as_str()?.to_owned(),
                        description: item["description"].as_str().unwrap_or_default().to_owned(),
                        source: item["source"].as_str().unwrap_or_default().to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

impl Ui {
    /// Names of the rows the list shows, in the order the arrow keys walk them.
    fn menu_names(&self) -> Vec<String> {
        view::suggestions(&self.commands, &self.input.value())
            .into_iter()
            .flat_map(|(_, items)| items.into_iter().map(|c| c.name.clone()))
            .collect()
    }

    /// Rebuilds the list from the prompt. Text goes in through the DOM, so command names and
    /// descriptions from the agent are never parsed as markup.
    fn render_menu(&self) -> Result<(), JsValue> {
        let groups = if self.menu_open {
            view::suggestions(&self.commands, &self.input.value())
        } else {
            Vec::new()
        };
        self.menu.set_text_content(None);
        let mut row = 0;
        for (source, items) in &groups {
            let group = self.document.create_element("div")?;
            group.set_attribute("role", "group")?;
            group.set_attribute("aria-label", source)?;
            let title = self.document.create_element("div")?;
            title.set_class_name("command-source");
            title.set_text_content(Some(source));
            group.append_child(&title)?;
            for command in items {
                let option = self.document.create_element("button")?;
                option.set_attribute("type", "button")?;
                option.set_attribute("role", "option")?;
                option.set_attribute("data-command", &command.name)?;
                option.set_attribute("aria-selected", &(row == self.selected).to_string())?;
                option.set_class_name("command");
                option.set_text_content(Some(&format!("/{}", command.name)));
                group.append_child(&option)?;
                row += 1;
            }
            self.menu.append_child(&group)?;
        }
        let highlighted = groups
            .iter()
            .flat_map(|(_, items)| items)
            .nth(self.selected);
        self.show_doc(highlighted.map(|command| command.description.as_str()))?;
        if groups.is_empty() {
            self.menu.set_attribute("hidden", "")
        } else {
            self.menu.remove_attribute("hidden")
        }
    }

    /// Shows `description` in the panel beside the list, or hides the panel when there is none.
    /// The input names the panel in `aria-describedby` only while it shows, so screen readers
    /// read the description when it appears and never a hidden element.
    fn show_doc(&self, description: Option<&str>) -> Result<(), JsValue> {
        match description.filter(|text| !text.is_empty()) {
            Some(text) => {
                self.command_doc.set_text_content(Some(text));
                self.input
                    .set_attribute("aria-describedby", "command-doc")?;
                self.command_doc.remove_attribute("hidden")
            }
            None => {
                self.input.remove_attribute("aria-describedby")?;
                self.command_doc.set_attribute("hidden", "")
            }
        }
    }

    /// Replaces the word being typed with `/<name> ` and closes the list.
    fn insert(&mut self, name: &str) -> Result<(), JsValue> {
        self.input
            .set_value(&view::with_command(&self.input.value(), name));
        self.menu_open = false;
        self.render_menu()?;
        self.input.focus()
    }

    /// Handles a key while the list is open. Returns whether the key was used, so the caller
    /// can stop it reaching the Enter-to-send handler.
    fn menu_key(&mut self, event: &KeyboardEvent) -> Result<bool, JsValue> {
        let names = self.menu_names();
        if !self.menu_open || names.is_empty() {
            return Ok(false);
        }
        let key = event.key();
        let count = names.len();
        let chosen = names[self.selected.min(count - 1)].clone();
        let used = match key.as_str() {
            "ArrowDown" => {
                self.selected = (self.selected + 1) % count;
                true
            }
            "ArrowUp" => {
                self.selected = (self.selected + count - 1) % count;
                true
            }
            "Escape" => {
                self.menu_open = false;
                true
            }
            "Tab" if !event.shift_key() => {
                self.insert(&chosen)?;
                true
            }
            _ if view::enter_sends(&key, event.shift_key(), event.is_composing()) => {
                self.insert(&chosen)?;
                true
            }
            _ => false,
        };
        self.render_menu()?;
        Ok(used)
    }
}

/// Shows the popover, asks the extension for the model's rows and moves focus into it.
fn open_profile(
    gear: &HtmlElement,
    pop: &HtmlElement,
    fields: &Element,
    api: &VsCodeApi,
) -> Result<(), JsValue> {
    fields.set_text_content(Some("Loading..."));
    pop.remove_attribute("hidden")?;
    gear.set_attribute("aria-expanded", "true")?;
    api.post_message(&json!({"type": "profile_show"}).to_string());
    pop.focus()
}

/// Hides the popover and returns focus to the gear that opened it.
fn close_profile(gear: &HtmlElement, pop: &HtmlElement) -> Result<(), JsValue> {
    pop.set_attribute("hidden", "")?;
    gear.set_attribute("aria-expanded", "false")?;
    gear.focus()
}

/// Posts one `profile_set` per field the user changed; an empty field keeps its value.
fn save_profile(fields: &Element, api: &VsCodeApi) -> Result<(), JsValue> {
    let inputs = fields.query_selector_all("input")?;
    for index in 0..inputs.length() {
        let Some(node) = inputs.item(index) else {
            continue;
        };
        let input: HtmlInputElement = node.dyn_into()?;
        let value = input.value();
        let value = value.trim();
        let Some(field) = input.get_attribute("data-field") else {
            continue;
        };
        if value.is_empty() || value == input.default_value() {
            continue;
        }
        api.post_message(
            &json!({"type": "profile_set", "field": field, "value": value}).to_string(),
        );
    }
    Ok(())
}

/// A disabled control carries the attribute, which browsers and screen readers both honour.
/// Whether the user asked the OS for less motion.
fn reduced_motion() -> bool {
    web_sys::window()
        .and_then(|window| {
            window
                .match_media("(prefers-reduced-motion: reduce)")
                .ok()
                .flatten()
        })
        .is_some_and(|query: MediaQueryList| query.matches())
}

fn set_enabled(element: &Element, enabled: bool) -> Result<(), JsValue> {
    if enabled {
        element.remove_attribute("disabled")
    } else {
        element.set_attribute("disabled", "")
    }
}

type Shared = Rc<RefCell<Ui>>;

fn set_hidden(element: &Element, hidden: bool) -> Result<(), JsValue> {
    if hidden {
        element.set_attribute("hidden", "")
    } else {
        element.remove_attribute("hidden")
    }
}

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
    let model: HtmlSelectElement = by_id(&document, "model")?.dyn_into()?;
    let mode: HtmlSelectElement = by_id(&document, "mode")?.dyn_into()?;
    let gear: HtmlElement = by_id(&document, "profile-gear")?.dyn_into()?;
    let profile: HtmlElement = by_id(&document, "profile-popover")?.dyn_into()?;
    let profile_fields = by_id(&document, "profile-fields")?;
    let profile_save: HtmlElement = by_id(&document, "profile-save")?.dyn_into()?;
    let menu = by_id(&document, "commands")?;
    let command_doc = by_id(&document, "command-doc")?;
    let queue_box = by_id(&document, "queue")?;
    let queue_status = by_id(&document, "queue-status")?;
    let queue_list = by_id(&document, "queue-list")?;
    let vscode = acquire_vscode_api();
    let ui: Shared = Rc::new(RefCell::new(Ui {
        document,
        output: output.clone(),
        phase: Phase::Idle,
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
        model: model.clone(),
        mode: mode.clone(),
        gear: gear.clone(),
        profile_fields: profile_fields.clone(),
        input: input.clone(),
        vscode: vscode.clone(),
        queue: queue::Queue::default(),
        queue_box: queue_box.clone(),
        queue_status: queue_status.clone(),
        queue_list: queue_list.clone(),
        commands: Vec::new(),
        menu: menu.clone(),
        command_doc: command_doc.clone(),
        menu_open: false,
        selected: 0,
        turns: 0,
        subagents: 0,
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

    // Registered before the Enter handler below: a key the list uses must not also send the prompt.
    let state = ui.clone();
    let on_menu_key = Closure::<dyn FnMut(KeyboardEvent)>::new(move |event: KeyboardEvent| {
        // A DOM failure here only leaves the list as it was; the panel stays usable.
        if let Ok(true) = state.borrow_mut().menu_key(&event) {
            event.prevent_default();
            event.stop_immediate_propagation();
        }
    });
    input.add_event_listener_with_callback("keydown", on_menu_key.as_ref().unchecked_ref())?;
    on_menu_key.forget();

    let state = ui.clone();
    let on_input = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
        let mut ui = state.borrow_mut();
        ui.menu_open = true;
        ui.selected = 0;
        let _ = ui.render_menu();
    });
    input.add_event_listener_with_callback("input", on_input.as_ref().unchecked_ref())?;
    on_input.forget();

    let state = ui.clone();
    let on_pick = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        if let Ok(Some(option)) = target.closest("[data-command]")
            && let Some(name) = option.get_attribute("data-command")
        {
            let _ = state.borrow_mut().insert(&name);
        }
    });
    menu.add_event_listener_with_callback("click", on_pick.as_ref().unchecked_ref())?;
    on_pick.forget();

    let state = ui.clone();
    let on_hover = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        let current = state.borrow();
        if let Ok(Some(option)) = target.closest("[data-command]")
            && let Some(name) = option.get_attribute("data-command")
            && let Some(command) = current.commands.iter().find(|c| c.name == name)
        {
            let _ = current.show_doc(Some(&command.description));
        }
    });
    menu.add_event_listener_with_callback("mouseover", on_hover.as_ref().unchecked_ref())?;
    on_hover.forget();

    // Leaving the list falls back to the highlighted row's description.
    let state = ui.clone();
    let on_leave = Closure::<dyn FnMut(Event)>::new(move |_: Event| {
        let _ = state.borrow().render_menu();
    });
    menu.add_event_listener_with_callback("mouseleave", on_leave.as_ref().unchecked_ref())?;
    on_leave.forget();

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

    let state = ui.clone();
    let on_submit = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        let text = input.value();
        let mut ui = state.borrow_mut();
        // Before the session is ready the text stays in the box; while a turn runs it joins the queue.
        if text.trim().is_empty() || !ui.ready {
            return;
        }
        if view::can_send(ui.ready, &ui.phase) {
            // A failed send leaves the turn unstarted; the prompt is already in the box to retry.
            let _ = ui.send(&text);
        } else {
            ui.queue.push(text);
            let _ = ui.render_queue();
        }
        input.set_value("");
    });
    form.add_event_listener_with_callback("submit", on_submit.as_ref().unchecked_ref())?;

    let (api, picker) = (vscode.clone(), model.clone());
    let on_model = Closure::<dyn FnMut()>::new(move || {
        // Disabled until the extension answers with the picker, so a second pick can't race the first.
        let _ = set_enabled(&picker, false);
        api.post_message(&json!({"type": "model", "key": picker.value()}).to_string());
    });
    model.add_event_listener_with_callback("change", on_model.as_ref().unchecked_ref())?;
    on_model.forget();

    let (api, picker) = (vscode.clone(), mode.clone());
    let on_mode = Closure::<dyn FnMut()>::new(move || {
        let _ = set_enabled(&picker, false);
        api.post_message(&json!({"type": "mode", "key": picker.value()}).to_string());
    });
    mode.add_event_listener_with_callback("change", on_mode.as_ref().unchecked_ref())?;
    on_mode.forget();

    let (api, gear_c, pop, fields) = (
        vscode.clone(),
        gear.clone(),
        profile.clone(),
        profile_fields.clone(),
    );
    let on_gear = Closure::<dyn FnMut()>::new(move || {
        let _ = if pop.has_attribute("hidden") {
            open_profile(&gear_c, &pop, &fields, &api)
        } else {
            close_profile(&gear_c, &pop)
        };
    });
    gear.add_event_listener_with_callback("click", on_gear.as_ref().unchecked_ref())?;
    on_gear.forget();

    let (api, fields) = (vscode.clone(), profile_fields.clone());
    let on_save = Closure::<dyn FnMut()>::new(move || {
        let _ = save_profile(&fields, &api);
    });
    profile_save.add_event_listener_with_callback("click", on_save.as_ref().unchecked_ref())?;
    on_save.forget();

    let (api, gear_c, pop, fields) = (
        vscode.clone(),
        gear.clone(),
        profile.clone(),
        profile_fields.clone(),
    );
    let on_profile_key = Closure::<dyn FnMut(KeyboardEvent)>::new(move |event: KeyboardEvent| {
        let typing = event
            .target()
            .is_some_and(|t| t.is_instance_of::<HtmlInputElement>());
        match event.key().as_str() {
            "Escape" => {
                event.prevent_default();
                let _ = close_profile(&gear_c, &pop);
            }
            // Enter saves here and never submits the chat form the popover sits in.
            "Enter" if typing => {
                event.prevent_default();
                let _ = save_profile(&fields, &api);
            }
            _ => {}
        }
    });
    profile.add_event_listener_with_callback("keydown", on_profile_key.as_ref().unchecked_ref())?;
    on_profile_key.forget();

    let api = vscode.clone();
    let on_stop = Closure::<dyn FnMut()>::new(move || {
        api.post_message(&json!({"type": "cancel"}).to_string());
    });
    stop.add_event_listener_with_callback("click", on_stop.as_ref().unchecked_ref())?;

    // Remove and Resume on the queue panel; a Resume that cannot send right now does nothing.
    let state = ui.clone();
    let on_queue = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(target) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        let mut ui = state.borrow_mut();
        if let Ok(Some(button)) = target.closest("[data-queue-remove]") {
            if let Some(index) = button
                .get_attribute("data-queue-remove")
                .and_then(|i| i.parse().ok())
            {
                ui.queue.remove(index);
            }
        } else if target.id() == "queue-resume" && view::can_send(ui.ready, &ui.phase) {
            // A failed send leaves the queue paused, so the owner can try again.
            if let Some(next) = ui.queue.resume() {
                let _ = ui.send(&next);
            }
        }
        let _ = ui.render_queue();
    });
    queue_box.add_event_listener_with_callback("click", on_queue.as_ref().unchecked_ref())?;
    on_queue.forget();

    // One delegated listener answers every permission prompt; buttons carry their request and option.
    let (api, state) = (vscode.clone(), ui.clone());
    let on_click = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        let Some(button) = event.target().and_then(|t| t.dyn_into::<Element>().ok()) else {
            return;
        };
        if let Some(path) = button.get_attribute("data-open-change") {
            api.post_message(&json!({"type": "open_change", "path": path}).to_string());
            return;
        }
        if let Some(path) = button.get_attribute("data-revert-file") {
            api.post_message(&json!({"type": "revert_file", "path": path}).to_string());
            return;
        }
        if let Some(turn) = button
            .get_attribute("data-turn")
            .and_then(|t| t.parse::<u64>().ok())
        {
            api.post_message(&json!({"type": "rewind", "turn": turn}).to_string());
            return;
        }
        if let Some(id) = button.get_attribute("data-stop-subagent") {
            api.post_message(&json!({"type": "stop_subagent", "id": id}).to_string());
            return;
        }
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
        if option.starts_with("reject") {
            state.borrow_mut().queue.pause();
        }
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

    // Dropped files arrive as `text/uri-list`; the extension filters them, so no checks here.
    let body = window
        .document()
        .ok_or("no document")?
        .body()
        .ok_or("no body")?;
    let (enter, leave, dropped) = (body.clone(), body.clone(), body);
    let on_dragover = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        let _ = enter.class_list().add_1("dragging");
    });
    window.add_event_listener_with_callback("dragover", on_dragover.as_ref().unchecked_ref())?;
    on_dragover.forget();
    let on_dragleave = Closure::<dyn FnMut()>::new(move || {
        let _ = leave.class_list().remove_1("dragging");
    });
    window.add_event_listener_with_callback("dragleave", on_dragleave.as_ref().unchecked_ref())?;
    on_dragleave.forget();
    let api = vscode.clone();
    let on_drop = Closure::<dyn FnMut(Event)>::new(move |event: Event| {
        event.prevent_default();
        let _ = dropped.class_list().remove_1("dragging");
        let Some(data) = event
            .dyn_into::<DragEvent>()
            .ok()
            .and_then(|drag| drag.data_transfer())
        else {
            return;
        };
        let uri_list = data.get_data("text/uri-list").unwrap_or_default();
        let text = if uri_list.is_empty() {
            data.get_data("text/plain").unwrap_or_default()
        } else {
            uri_list
        };
        let uris: Vec<&str> = text.lines().collect();
        api.post_message(&json!({"type": "dropped", "uris": uris}).to_string());
    });
    window.add_event_listener_with_callback("drop", on_drop.as_ref().unchecked_ref())?;
    on_drop.forget();

    let state = ui;
    let tick = Closure::<dyn FnMut()>::new(move || state.borrow().update_working());
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

/// Runs a change to the transcript, then scrolls to the bottom only if the reader was already there.
fn keep_bottom<T>(
    output: &Element,
    change: impl FnOnce() -> Result<T, JsValue>,
) -> Result<T, JsValue> {
    let follow = view::at_bottom(
        f64::from(output.scroll_top()),
        f64::from(output.client_height()),
        f64::from(output.scroll_height()),
    );
    let value = change()?;
    if follow {
        output.set_scroll_top(output.scroll_height());
    }
    Ok(value)
}

impl Ui {
    fn append(&self, tag: &str, class: &str, text: &str) -> Result<Element, JsValue> {
        keep_bottom(&self.output, || {
            let element = self.document.create_element(tag)?;
            element.set_class_name(class);
            element.set_text_content(Some(text));
            self.output.append_child(&element)?;
            Ok(element)
        })
    }

    /// A user bubble, with the button that rewinds the chat to before it.
    fn user_bubble(&mut self, prompt: &str) -> Result<(), JsValue> {
        self.turns += 1;
        keep_bottom(&self.output, || {
            self.append("p", "user", prompt)?;
            let button: HtmlElement = self.document.create_element("button")?.dyn_into()?;
            button.set_attribute("type", "button")?;
            button.set_attribute("data-turn", &self.turns.to_string())?;
            button.set_text_content(Some("Rewind to here"));
            self.output.append_child(&button)?;
            Ok(())
        })
    }

    /// Drops the chat from user turn `turn` on, then notes that Bash effects stay.
    fn rewound(&mut self, turn: usize) -> Result<(), JsValue> {
        let children = self.output.query_selector_all(":scope > *")?;
        let mut elements = Vec::new();
        for index in 0..children.length() {
            if let Some(element) = children
                .item(index)
                .and_then(|n| n.dyn_into::<Element>().ok())
            {
                elements.push(element);
            }
        }
        let classes: Vec<String> = elements.iter().map(Element::class_name).collect();
        if let Some(cut) = view::cut_at_turn(&classes, turn) {
            for element in &elements[cut..] {
                element.remove();
            }
        }
        self.turns = turn - 1;
        self.message = None;
        let note = format!("Rewound to before turn {turn}. Bash effects are not undone.");
        self.append("p", "notice", &note)?;
        Ok(())
    }

    /// Posts a prompt the way a normal send does, then starts the turn it opens.
    fn send(&mut self, text: &str) -> Result<(), JsValue> {
        let message = json!({"type": "prompt", "text": text, "file": self.attached_file(),
            "files": self.added, "problems": self.problems.checked()});
        self.vscode.post_message(&message.to_string());
        self.begin(text)
    }

    /// Shows the queued prompts above the input, each with its own Remove button.
    fn render_queue(&self) -> Result<(), JsValue> {
        let prompts = self.queue.prompts();
        set_hidden(&self.queue_box, prompts.is_empty())?;
        set_hidden(&self.queue_status, !self.queue.is_paused())?;
        self.queue_list.set_text_content(Some(""));
        for (index, prompt) in prompts.iter().enumerate() {
            let item = self.document.create_element("li")?;
            let text = self.document.create_element("span")?;
            text.set_text_content(Some(prompt));
            let remove = self.document.create_element("button")?;
            remove.set_attribute("type", "button")?;
            remove.set_attribute("data-queue-remove", &index.to_string())?;
            remove.set_attribute("aria-label", &format!("Remove queued prompt: {prompt}"))?;
            remove.set_text_content(Some("Remove"));
            item.append_child(&text)?;
            item.append_child(&remove)?;
            self.queue_list.append_child(&item)?;
        }
        Ok(())
    }

    fn begin(&mut self, prompt: &str) -> Result<(), JsValue> {
        self.message = None;
        self.user_bubble(prompt)?;
        self.started = js_sys::Date::now();
        self.ended = None;
        self.phase = Phase::ProcessingPrompt;
        self.output.set_attribute("aria-busy", "true")?;
        // Static markup with no user text; CSS draws the animation and `update_working` fills the word.
        let indicator = self.document.create_element("div")?;
        indicator.set_id("working");
        indicator.set_class_name("working");
        indicator.set_inner_html(
            r#"<span class="working-pac" aria-hidden="true"><span class="working-man"></span><span class="working-dots"><i></i><i></i><i></i><i></i></span></span><span id="working-word" class="working-word" aria-live="polite"></span>"#,
        );
        keep_bottom(&self.output, || {
            self.output.append_child(&indicator).map(drop)
        })?;
        set_enabled(&self.stop, true)?;
        self.sync_pickers()?;
        self.update_working();
        Ok(())
    }

    /// Leaves the busy state, so a dead or failed session never looks like it is still working.
    fn idle(&mut self) -> Result<(), JsValue> {
        self.message = None;
        self.phase = Phase::Idle;
        self.ended = Some(js_sys::Date::now());
        self.output.remove_attribute("aria-busy")?;
        if let Some(indicator) = self.document.get_element_by_id("working") {
            indicator.remove();
        }
        set_enabled(&self.stop, false)?;
        self.sync_pickers()
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
        if ready {
            self.sync_pickers()
        } else {
            self.idle()
        }
    }

    /// The pickers can change only between turns, once the session is ready and has options.
    fn sync_pickers(&self) -> Result<(), JsValue> {
        let idle = self.phase == Phase::Idle;
        let open = self.ready && idle;
        set_enabled(&self.model, open && self.model.length() > 0)?;
        set_enabled(&self.mode, open && self.mode.length() > 0)?;
        set_enabled(&self.gear, open && self.model.length() > 0)
    }

    /// Fills the popover with the `profile` message's rows: a labelled input per field, its source as text.
    fn render_profile(&self, message: &Value) -> Result<(), JsValue> {
        self.profile_fields.set_text_content(Some(""));
        for row in message["rows"].as_array().into_iter().flatten() {
            let field = row["field"].as_str().unwrap_or_default();
            let id = format!("profile-{field}");
            let line = self.document.create_element("div")?;
            line.set_class_name("profile-row");
            let label = self.document.create_element("label")?;
            label.set_attribute("for", &id)?;
            label.set_text_content(Some(field));
            let input: HtmlInputElement = self.document.create_element("input")?.dyn_into()?;
            let text = view::profile_text(&row["value"]);
            input.set_id(&id);
            input.set_attribute("data-field", field)?;
            input.set_attribute("placeholder", "unset")?;
            input.set_default_value(&text);
            input.set_value(&text);
            let source = self.document.create_element("span")?;
            source.set_id(&format!("{id}-source"));
            source.set_class_name("profile-source");
            source.set_text_content(Some(row["source"].as_str().unwrap_or_default()));
            input.set_attribute("aria-describedby", &format!("{id}-source"))?;
            line.append_child(&label)?;
            line.append_child(&input)?;
            line.append_child(&source)?;
            self.profile_fields.append_child(&line)?;
        }
        Ok(())
    }

    /// Fills `select` from the extension's `models` or `modes` message and selects the current option.
    fn fill_picker(&mut self, select: &HtmlSelectElement, message: &Value) -> Result<(), JsValue> {
        select.set_inner_html("");
        for option in message["options"].as_array().into_iter().flatten() {
            let element: HtmlOptionElement = self.document.create_element("option")?.dyn_into()?;
            element.set_value(option["value"].as_str().unwrap_or_default());
            element.set_text_content(option["name"].as_str());
            select.append_child(&element)?;
        }
        select.set_value(message["current"].as_str().unwrap_or_default());
        self.sync_pickers()
    }

    fn update_working(&self) {
        let end = self.ended.unwrap_or_else(js_sys::Date::now);
        let elapsed = ((end - self.started) / 1000.0).max(0.0) as u64;
        self.set_working_word(elapsed);
    }

    /// Cycles the busy indicator's word. Writes only on change, so a screen reader announces each change once.
    fn set_working_word(&self, elapsed: u64) {
        let Some(word) = self.document.get_element_by_id("working-word") else {
            return;
        };
        let text = format!(
            "{}\u{2026}",
            view::working_word(elapsed, self.turns, reduced_motion())
        );
        if word.text_content().as_deref() != Some(text.as_str()) {
            word.set_text_content(Some(&text));
        }
    }

    fn handle(&mut self, message: &Value) -> Result<(), JsValue> {
        let text = |key: &str| message[key].as_str().unwrap_or_default();
        match text("type") {
            "notice" => {
                // These texts come from `praxis-vscode/src/lib.rs` and `protocol.rs`.
                let notice = text("text");
                if notice == "ready" {
                    self.set_ready(true)?;
                } else if notice.starts_with("praxis acp")
                    || notice.starts_with("Praxis could not start")
                {
                    self.set_ready(false)?;
                }
                self.append("p", "notice", notice)?;
            }
            // A replayed prompt: plain text, and the next reply starts a new message.
            "commands" => {
                self.commands = parse_commands(message);
                self.selected = 0;
                self.render_menu()?;
            }
            "user" => {
                self.message = None;
                self.user_bubble(text("text"))?;
            }
            "reverted" => {
                if let Some(path) = message["path"].as_str() {
                    self.mark_reverted(path)?;
                }
            }
            "rewound" => {
                let turn = message["turn"]
                    .as_u64()
                    .and_then(|t| usize::try_from(t).ok());
                if let Some(turn) = turn.filter(|turn| *turn >= 1) {
                    self.rewound(turn)?;
                }
            }
            "text" => {
                self.phase = Phase::Generating;
                if self.message.is_none() {
                    let element = self.append("div", "message", "")?;
                    self.message = Some((element, String::new()));
                }
                if let Some((element, markdown)) = &mut self.message {
                    markdown.push_str(text("text"));
                    keep_bottom(&self.output, || {
                        // Safe only because `view::markdown` escapes raw HTML and drops unsafe links.
                        element.set_inner_html(&view::markdown(markdown));
                        Ok(())
                    })?;
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
            "models" => {
                let model = self.model.clone();
                self.fill_picker(&model, message)?;
            }
            "modes" => {
                let mode = self.mode.clone();
                self.fill_picker(&mode, message)?;
            }
            "profile" => self.render_profile(message)?,
            "tool" => self.tool(message)?,
            "subagent_usage" => self.subagent_usage(message)?,
            "permission" => self.permission(message)?,
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
                if let Some(next) = self.queue.turn_ended(text("reason")) {
                    self.send(&next)?;
                }
                self.render_queue()?;
            }
            _ => return Ok(()),
        }
        self.update_working();
        Ok(())
    }

    fn tool(&mut self, message: &Value) -> Result<(), JsValue> {
        self.message = None;
        let id = message["id"].as_str().unwrap_or_default().to_owned();
        let status = message["status"].as_str().unwrap_or("pending");
        self.phase = match status {
            "pending" | "in_progress" => Phase::RunningTool,
            _ => Phase::ProcessingPrompt,
        };
        if !self.tools.contains_key(&id) {
            // Collapsed by default: tool output is long and the summary carries the status.
            let details = self.document.create_element("details")?;
            details.set_class_name("tool");
            let summary = self.document.create_element("summary")?;
            let pre = self.document.create_element("pre")?;
            details.append_child(&summary)?;
            details.append_child(&pre)?;
            // A subagent's call nests under the `Agent` call that runs it.
            keep_bottom(&self.output, || {
                match message["parent"]
                    .as_str()
                    .and_then(|parent| self.tools.get_mut(parent))
                {
                    Some(owner) => {
                        owner.calls += 1;
                        owner.children.push(id.clone());
                        owner.details.append_child(&details)?;
                        show_summary(owner)
                    }
                    None => self.output.append_child(&details).map(drop),
                }
            })?;
            let tool = Tool {
                details,
                summary,
                pre,
                title: "tool".to_owned(),
                status: String::new(),
                kind: None,
                name: None,
                calls: 0,
                usage: None,
                file: None,
                parent: message["parent"].as_str().map(str::to_owned),
                children: Vec::new(),
                files: None,
                stop: None,
            };
            self.tools.insert(id.clone(), tool);
        }
        let Some(tool) = self.tools.get_mut(&id) else {
            return Ok(());
        };
        if let Some(title) = message["title"].as_str() {
            tool.title = title.to_owned();
        }
        if let Some(kind) = message["subagent"].as_str() {
            if tool.kind.is_none() {
                tool.name = Some(view::subagent_name(self.subagents));
                self.subagents += 1;
            }
            tool.kind = Some(kind.to_owned());
        }
        if tool.kind.is_some() && tool.stop.is_none() {
            let stop = self.document.create_element("button")?;
            stop.set_attribute("type", "button")?;
            stop.set_attribute("data-stop-subagent", &id)?;
            stop.set_text_content(Some("Stop"));
            tool.details.insert_before(&stop, Some(tool.pre.as_ref()))?;
            tool.stop = Some(stop);
        }
        if tool.kind.is_some() && tool.files.is_none() {
            let files = self.document.create_element("div")?;
            files.set_attribute("role", "group")?;
            files.set_attribute("aria-label", "Changed files")?;
            tool.details
                .insert_before(&files, Some(tool.pre.as_ref()))?;
            tool.files = Some(files);
        }
        if let Some(file) = message["file"].as_str() {
            if tool.file.is_none() {
                // Beside the collapsed block, so it shows without opening the call.
                let open_diff = self.document.create_element("button")?;
                open_diff.set_attribute("type", "button")?;
                open_diff.set_attribute("data-open-change", file)?;
                open_diff.set_text_content(Some("Open diff"));
                let revert = self.document.create_element("button")?;
                revert.set_attribute("type", "button")?;
                revert.set_attribute("data-revert-file", file)?;
                revert.set_text_content(Some("Revert file"));
                if let Some(parent) = tool.details.parent_node() {
                    let after = tool.details.next_sibling();
                    parent.insert_before(&open_diff, after.as_ref())?;
                    parent.insert_before(&revert, after.as_ref())?;
                }
            }
            tool.file = Some(file.to_owned());
        }
        status.clone_into(&mut tool.status);
        show_summary(tool)?;
        if let Some(output) = message["output"].as_str() {
            tool.pre.set_text_content(Some(output));
        }
        let parent = tool.parent.clone();
        match parent {
            Some(parent) => self.refresh_files(&parent),
            None => Ok(()),
        }
    }

    /// Turns every "Revert file" button for `path` into a disabled "Reverted", since each
    /// revert restores the file to before the whole change set.
    fn mark_reverted(&self, path: &str) -> Result<(), JsValue> {
        let buttons = self.document.query_selector_all("[data-revert-file]")?;
        for index in 0..buttons.length() {
            let Some(button) = buttons
                .item(index)
                .and_then(|n| n.dyn_into::<Element>().ok())
            else {
                continue;
            };
            if button.get_attribute("data-revert-file").as_deref() == Some(path) {
                button.set_text_content(Some("Reverted"));
                button.set_attribute("disabled", "")?;
            }
        }
        Ok(())
    }

    /// Rebuilds an `Agent` call's changed-files buttons from its calls, so a file lists once its
    /// `Write` or `Edit` completes. Each button carries the path the click opens.
    fn refresh_files(&self, owner: &str) -> Result<(), JsValue> {
        let Some(tool) = self.tools.get(owner) else {
            return Ok(());
        };
        let Some(container) = &tool.files else {
            return Ok(());
        };
        let calls: Vec<(Option<String>, String)> = tool
            .children
            .iter()
            .filter_map(|id| self.tools.get(id))
            .map(|call| (call.file.clone(), call.status.clone()))
            .collect();
        let files = view::changed_files(
            calls
                .iter()
                .map(|(file, status)| (file.as_deref(), status.as_str())),
        );
        container.set_text_content(None);
        for file in files {
            let button = self.document.create_element("button")?;
            button.set_attribute("type", "button")?;
            button.set_attribute("data-open-change", file)?;
            button.set_text_content(Some(file));
            container.append_child(&button)?;
        }
        Ok(())
    }

    /// Sets the prompt size on an `Agent` call's block; the summary line shows it.
    fn subagent_usage(&mut self, message: &Value) -> Result<(), JsValue> {
        let id = message["id"].as_str().unwrap_or_default();
        let (Some(prompt), Some(context)) = (
            message["promptTokens"].as_u64(),
            message["contextLength"].as_u64(),
        ) else {
            return Ok(());
        };
        if let Some(tool) = self.tools.get_mut(id) {
            tool.usage = u32::try_from(prompt).ok().zip(u32::try_from(context).ok());
            show_summary(tool)?;
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
