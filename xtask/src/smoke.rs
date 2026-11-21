//! `cargo xtask vscode-smoke`: a real VS Code, the built extension, and the fake model server
//! (SPEC §14). It needs a display (`xvfb-run`) and is kept out of `scripts/verify`.

use crate::{run, vsix, workspace_root};
use anyhow::{Context, Result, bail};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const MODEL: &str = "m";
const PROMPT: &str = "Say hi";
const REPLY: &str = "Hello from the fake model";
/// Our UI document: VS Code nests the webview's content in the host frame's `active-frame`.
const FRAME: &str = "(document.getElementById('active-frame')?.contentDocument ?? document)";
const OVERALL: Duration = Duration::from_secs(240);

pub async fn vscode_smoke() -> Result<()> {
    if let Some(tool) = missing_tool(&env::var_os("PATH").unwrap_or_default()) {
        bail!(
            "`{tool}` not on PATH; install VS Code and `xorg-server-xvfb` (pacman -S xorg-server-xvfb)"
        );
    }
    let root = workspace_root()?;
    vsix()?;
    // The extension finds `praxis` on PATH (`praxis.binaryPath` is machine-scoped).
    run(std::process::Command::new("cargo")
        .current_dir(root)
        .args(["build", "-p", "praxis-cli"]))?;

    let tmp = root.join("target/vscode-smoke");
    let _ = fs::remove_dir_all(&tmp);
    let ext = root.join("target/vsix");
    fs::copy(
        root.join("editors/vscode/package.json"),
        ext.join("package.json"),
    )?;
    // Electron's IPC socket lives under the profile, and a socket path is capped at 108 bytes.
    let data = env::temp_dir().join(format!("pxsmoke-{}", std::process::id()));
    let _ = fs::remove_dir_all(&data);
    let server = fake_server().await;
    let Profile {
        home,
        xdg,
        exts,
        ws,
    } = profile(&tmp, &server.uri())?;

    let port = TcpListener::bind("127.0.0.1:0")?.local_addr()?.port();
    let log = tmp.join("code.log");
    let path = format!(
        "{}:{}",
        root.join("target/debug").display(),
        env::var("PATH").unwrap_or_default()
    );
    // `process_group(0)` lets `stop` take down xvfb-run's Xvfb along with VS Code.
    let mut code = Command::new("xvfb-run")
        // `--wait` keeps the CLI alive: without it `code` returns at once and xvfb-run kills the display.
        .args(["-a", "code", "--wait", "--user-data-dir"])
        .arg(&data)
        .arg("--extensions-dir")
        .arg(&exts)
        .arg("--extensionDevelopmentPath")
        .arg(&ext)
        .arg("--disable-extensions")
        .arg(format!("--remote-debugging-port={port}"))
        .arg(&ws)
        .env_clear()
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("PATH", path)
        .stdin(Stdio::null())
        .stdout(File::create(tmp.join("code.out"))?)
        .stderr(File::create(&log)?)
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .context("failed to start xvfb-run")?;

    let result = match tokio::time::timeout(OVERALL, drive(port, &mut code)).await {
        Ok(result) => result,
        Err(_) => Err(anyhow::anyhow!("timed out after {OVERALL:?}")),
    };
    stop(&mut code).await;
    let _ = fs::remove_dir_all(&data);
    result.with_context(|| {
        format!(
            "vscode-smoke failed; VS Code output is in {}",
            log.display()
        )
    })?;
    eprintln!("vscode-smoke: ok (reply rendered in the Praxis webview)");
    Ok(())
}

async fn drive(port: u16, code: &mut Child) -> Result<()> {
    let until = Instant::now() + Duration::from_secs(60);
    let page = wait_target(port, code, until, is_workbench)
        .await?
        .context("no VS Code workbench target on the debugging port")?;
    eprintln!("vscode-smoke: workbench up, opening the chat");
    let mut workbench = Cdp::connect(&page).await?;
    // The keystrokes can land before VS Code takes focus, so repeat them until a webview appears.
    let webview = loop {
        open_chat(&mut workbench).await?;
        let until = Instant::now() + Duration::from_secs(20);
        if let Some(url) = wait_target(port, code, until, is_webview).await? {
            break url;
        }
    };
    eprintln!("vscode-smoke: chat webview found, sending the prompt");
    send_prompt(&mut Cdp::connect(&webview).await?, 1).await?;
    // An extension host restart kills the agent; the panel must come back wired to a new one.
    eprintln!("vscode-smoke: restarting the extension host");
    run_command(&mut workbench, "Developer: Restart Extension Host").await?;
    let webview = restored_chat(port, &webview).await?;
    eprintln!("vscode-smoke: sending a prompt to the restored chat");
    // The restored session replays the first reply, so a second one proves the new agent answers.
    send_prompt(&mut Cdp::connect(&webview).await?, 2).await
}

/// The chat reopened after the restart: a new webview whose transcript replays the first reply.
/// VS Code can leave the dead panel's target listed for a moment, so `old` is skipped.
async fn restored_chat(port: u16, old: &str) -> Result<String> {
    let until = Instant::now() + Duration::from_secs(60);
    while Instant::now() < until {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(Ok(list)) =
            tokio::time::timeout(Duration::from_secs(5), http_json(port, "/json/list")).await
        else {
            continue;
        };
        let urls = list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|t| is_webview(t));
        for url in urls.filter_map(|t| t["webSocketDebuggerUrl"].as_str()) {
            if url == old {
                continue;
            }
            let Ok(mut ui) = Cdp::connect(url).await else {
                continue;
            };
            let shown = ui
                .eval(&format!(
                    "{FRAME}.getElementById('output')?.textContent ?? ''"
                ))
                .await?;
            if shown.as_str().is_some_and(|text| text.contains(REPLY)) {
                return Ok(url.to_owned());
            }
        }
    }
    bail!("no chat reopened with the session after the extension host restart")
}

/// Polls `/json/list` until a target matches. `None` on timeout; an early VS Code exit is an error.
async fn wait_target(
    port: u16,
    code: &mut Child,
    until: Instant,
    matches: fn(&Value) -> bool,
) -> Result<Option<String>> {
    loop {
        if let Some(status) = code.try_wait()? {
            bail!("VS Code exited early with {status}");
        }
        let fetch = tokio::time::timeout(Duration::from_secs(5), http_json(port, "/json/list"));
        if let Ok(Ok(list)) = fetch.await
            && let Some(url) = pick_target(&list, matches)
        {
            return Ok(Some(url));
        }
        if Instant::now() >= until {
            return Ok(None);
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

fn is_workbench(target: &Value) -> bool {
    target["type"] == "page"
        && target["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("vscode-file://"))
}

/// Our webview's own document, whose title is set in `editors/vscode/webview.html`.
fn is_webview(target: &Value) -> bool {
    target["title"] == "Praxis"
        || target["url"]
            .as_str()
            .is_some_and(|u| u.starts_with("vscode-webview://"))
}

fn pick_target(list: &Value, matches: fn(&Value) -> bool) -> Option<String> {
    list.as_array()?.iter().find(|target| matches(target))?["webSocketDebuggerUrl"]
        .as_str()
        .map(str::to_owned)
}

async fn open_chat(workbench: &mut Cdp) -> Result<()> {
    run_command(workbench, "Praxis: Open Chat").await
}

/// Ctrl+Shift+P, then the command's title and Enter: the same path a user takes.
async fn run_command(workbench: &mut Cdp, title: &str) -> Result<()> {
    let modifiers = 2 | 8; // CDP bit flags: Ctrl, Shift
    let p =
        json!({"key": "P", "code": "KeyP", "windowsVirtualKeyCode": 80, "modifiers": modifiers});
    workbench.key("keyDown", p.clone()).await?;
    workbench.key("keyUp", p).await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    workbench
        .call("Input.insertText", json!({"text": title}))
        .await?;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let enter = json!({"key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13});
    workbench
        .key(
            "keyDown",
            json!({"key": "Enter", "code": "Enter", "windowsVirtualKeyCode": 13, "text": "\r"}),
        )
        .await?;
    workbench.key("keyUp", enter).await
}

/// Types into our own composer and submits it, until the transcript holds `replies` replies. The UI
/// keeps the text until the session is ready, so the submit is repeated while the box still holds it.
async fn send_prompt(ui: &mut Cdp, replies: usize) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut sent = false;
    loop {
        let state = ui
            .eval(&format!("({{text: {FRAME}.getElementById('prompt')?.value ?? '', output: {FRAME}.getElementById('output')?.textContent ?? ''}})"))
            .await?;
        let output = state["output"].as_str().unwrap_or_default();
        if output.matches(REPLY).count() >= replies {
            return Ok(());
        }
        if Instant::now() >= deadline {
            let shown: String = output.chars().take(300).collect();
            bail!("the reply never rendered in the webview; transcript: {shown:?}");
        }
        if state["text"].as_str().is_some_and(|t| !t.is_empty()) || !sent {
            ui.eval(&format!(
                "(() => {{ {FRAME}.getElementById('prompt').value = {}; {FRAME}.getElementById('prompt-form').requestSubmit(); }})()",
                json!(PROMPT)
            ))
            .await?;
            sent = true;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn fake_server() -> MockServer {
    let server = MockServer::start().await;
    let models = json!({"models": [{
        "key": MODEL, "type": "llm", "max_context_length": 32768,
        "loaded_instances": [{"id": format!("{MODEL}:1"), "config": {"context_length": 32768}}],
        "capabilities": {"trained_for_tool_use": true},
    }]});
    Mock::given(method("GET"))
        .and(path("/api/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(models))
        .mount(&server)
        .await;
    let sse = format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"delta": {"content": REPLY}, "finish_reason": null}]}),
        json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .mount(&server)
        .await;
    server
}

/// Stops VS Code and everything in its process group (xvfb-run's Xvfb included).
async fn stop(code: &mut Child) {
    if let Some(pid) = code.id() {
        let _ = std::process::Command::new("kill")
            .args(["-TERM", "--", &format!("-{pid}")])
            .status();
    }
    let _ = code.kill().await;
}

fn missing_tool(path: &OsStr) -> Option<&'static str> {
    ["xvfb-run", "code"]
        .into_iter()
        .find(|tool| !on_path(tool, path))
}

struct Profile {
    home: PathBuf,
    xdg: PathBuf,
    exts: PathBuf,
    ws: PathBuf,
}

/// Lays out the isolated home, config (pointing at the fake server) and fixture workspace.
fn profile(tmp: &Path, server: &str) -> Result<Profile> {
    let profile = Profile {
        home: tmp.join("home"),
        xdg: tmp.join("xdg"),
        exts: tmp.join("ext"),
        ws: tmp.join("workspace"),
    };
    for dir in [
        &profile.xdg.join("praxis"),
        &profile.exts,
        &profile.ws,
        &profile.home,
    ] {
        fs::create_dir_all(dir)?;
    }
    fs::write(profile.ws.join("README.md"), "# Smoke fixture\n")?;
    let config = format!("default_model = \"{MODEL}\"\n\n[lmstudio]\nbase_url = \"{server}\"\n");
    fs::write(profile.xdg.join("praxis/config.toml"), config)?;
    Ok(profile)
}

fn on_path(tool: &str, path: &OsStr) -> bool {
    env::split_paths(path).any(|dir| dir.join(tool).is_file())
}

/// `GET` over a bare socket. DevTools keeps the connection open after the body, so the read stops
/// at `Content-Length` rather than at EOF.
async fn http_json(port: u16, route: &str) -> Result<Value> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await?;
    stream
        .write_all(format!("GET {route} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes())
        .await?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    while !response_complete(&raw) {
        let read = stream.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
    }
    Ok(serde_json::from_slice(http_body(&raw)?)?)
}

fn http_body(raw: &[u8]) -> Result<&[u8]> {
    let at = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .context("HTTP response has no header end")?;
    Ok(&raw[at + 4..])
}

fn response_complete(raw: &[u8]) -> bool {
    let Ok(body) = http_body(raw) else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..raw.len() - body.len()]).to_ascii_lowercase();
    head.lines()
        .find_map(|line| line.strip_prefix("content-length:"))
        .and_then(|length| length.trim().parse::<usize>().ok())
        .is_some_and(|length| body.len() >= length)
}

/// Minimal Chrome DevTools Protocol client: JSON-RPC over one websocket per target.
struct Cdp {
    ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
    next_id: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Self> {
        let (ws, _) = connect_async(url)
            .await
            .with_context(|| format!("CDP websocket {url}"))?;
        Ok(Self { ws, next_id: 0 })
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        self.next_id += 1;
        let id = self.next_id;
        let request = json!({"id": id, "method": method, "params": params});
        self.ws.send(Message::text(request.to_string())).await?;
        loop {
            let message = self.ws.next().await.context("CDP socket closed")??;
            let Message::Text(text) = message else {
                continue;
            };
            let reply: Value = serde_json::from_str(&text)?;
            if reply["id"] != id {
                continue; // an event, not our answer
            }
            if let Some(error) = reply.get("error") {
                bail!("{method} failed: {error}");
            }
            return Ok(reply["result"].clone());
        }
    }

    async fn key(&mut self, kind: &str, mut params: Value) -> Result<()> {
        params["type"] = kind.into();
        self.call("Input.dispatchKeyEvent", params).await.map(drop)
    }

    async fn eval(&mut self, expression: &str) -> Result<Value> {
        let reply = self
            .call(
                "Runtime.evaluate",
                json!({"expression": expression, "returnByValue": true}),
            )
            .await?;
        if let Some(thrown) = reply.get("exceptionDetails") {
            bail!("page script threw: {thrown}");
        }
        Ok(reply["result"]["value"].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picks_the_first_target_that_matches() {
        let list = json!([
            {"type": "page", "url": "vscode-file://x/workbench.html", "webSocketDebuggerUrl": "ws://a"},
            {"type": "iframe", "title": "Praxis", "url": "about:blank", "webSocketDebuggerUrl": "ws://b"},
        ]);
        assert_eq!(pick_target(&list, is_workbench).as_deref(), Some("ws://a"));
        assert_eq!(pick_target(&list, is_webview).as_deref(), Some("ws://b"));
    }

    #[test]
    fn no_matching_target_is_none() {
        let list =
            json!([{"type": "page", "url": "about:blank", "webSocketDebuggerUrl": "ws://c"}]);
        assert_eq!(pick_target(&list, is_webview), None);
        assert_eq!(pick_target(&json!({}), is_workbench), None);
    }

    #[test]
    fn body_follows_the_header_end() {
        let raw = b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n[1]";
        assert_eq!(http_body(raw).unwrap(), b"[1]");
        assert!(http_body(b"HTTP/1.0 200 OK").is_err());
    }

    #[test]
    fn a_response_is_complete_at_its_content_length() {
        let head = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n";
        assert!(!response_complete(&[head.as_slice(), b"["].concat()));
        assert!(response_complete(&[head.as_slice(), b"[]"].concat()));
    }

    #[tokio::test]
    async fn reads_json_from_a_server_that_keeps_the_connection_open() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0u8; 1024];
            let _ = socket.read(&mut request).await.unwrap();
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\n[{\"a\":1}]")
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_secs(5)).await; // never closes in time
        });
        let value = http_json(port, "/json/list").await.unwrap();
        assert_eq!(value, json!([{"a": 1}]));
        server.abort();
    }

    #[tokio::test]
    async fn cdp_skips_events_and_returns_the_matching_answer() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let request: Value = serde_json::from_str(&text).unwrap();
                let id = request["id"].clone();
                let event = json!({"method": "Page.loadEventFired"});
                ws.send(Message::text(event.to_string())).await.unwrap();
                let reply = match request["params"]["expression"].as_str() {
                    Some("boom") => json!({"id": id, "result": {"exceptionDetails": {}}}),
                    Some("fail") => json!({"id": id, "error": {"message": "no"}}),
                    _ => json!({"id": id, "result": {"result": {"value": 42}}}),
                };
                ws.send(Message::text(reply.to_string())).await.unwrap();
            }
        });
        let mut cdp = Cdp::connect(&format!("ws://127.0.0.1:{port}"))
            .await
            .unwrap();
        assert_eq!(cdp.eval("1").await.unwrap(), json!(42));
        assert!(cdp.eval("boom").await.is_err());
        assert!(cdp.eval("fail").await.is_err());
        cdp.key("keyDown", json!({})).await.unwrap();
    }

    /// A DevTools stand-in: `/json/list` names a workbench and our webview, and every CDP call
    /// answers as if the reply had already rendered.
    async fn fake_devtools() -> u16 {
        let ws = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_url = format!("ws://{}", ws.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((socket, _)) = ws.accept().await {
                tokio::spawn(async move {
                    let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
                    while let Some(Ok(Message::Text(text))) = ws.next().await {
                        let request = serde_json::from_str::<Value>(&text).unwrap();
                        let id = request["id"].clone();
                        // `send_prompt` reads the composer and transcript; `restored_chat` the transcript only.
                        let expression =
                            request["params"]["expression"].as_str().unwrap_or_default();
                        let value = if expression.contains("'prompt')?.value") {
                            json!({"text": "", "output": format!("{REPLY}{REPLY}")})
                        } else {
                            json!(REPLY)
                        };
                        let reply = json!({"id": id, "result": {"result": {"value": value}}});
                        ws.send(Message::text(reply.to_string())).await.unwrap();
                    }
                });
            }
        });
        let list = json!([
            {"type": "page", "url": "vscode-file://w", "webSocketDebuggerUrl": ws_url},
            {"type": "iframe", "title": "Praxis", "url": "", "webSocketDebuggerUrl": ws_url},
            // The chat reopened after the extension host restart.
            {"type": "iframe", "title": "Praxis", "url": "", "webSocketDebuggerUrl": format!("{ws_url}/2")},
        ])
        .to_string();
        let http = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = http.local_addr().unwrap().port();
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = http.accept().await {
                let mut request = [0u8; 1024];
                let _ = socket.read(&mut request).await;
                let head = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", list.len());
                let _ = socket.write_all(format!("{head}{list}").as_bytes()).await;
            }
        });
        port
    }

    #[tokio::test]
    async fn drives_the_chat_until_the_reply_renders() {
        let port = fake_devtools().await;
        let mut code = tokio::process::Command::new("sleep")
            .arg("60")
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        drive(port, &mut code).await.unwrap();
    }

    #[test]
    fn names_the_first_missing_tool_and_writes_the_profile() {
        assert_eq!(
            missing_tool(OsStr::new("/nonexistent-dir")),
            Some("xvfb-run")
        );
        let tmp = env::temp_dir().join(format!("smoke-profile-{}", std::process::id()));
        let profile = profile(&tmp, "http://127.0.0.1:9").unwrap();
        let config = fs::read_to_string(profile.xdg.join("praxis/config.toml")).unwrap();
        assert!(config.contains("base_url = \"http://127.0.0.1:9\""));
        assert!(profile.ws.join("README.md").is_file() && profile.home.is_dir());
        assert!(profile.exts.is_dir());
        fs::remove_dir_all(&tmp).unwrap();
    }

    #[tokio::test]
    async fn stop_ends_the_process_group() {
        let mut code = Command::new("sleep")
            .arg("60")
            .process_group(0)
            .spawn()
            .unwrap();
        stop(&mut code).await;
        assert!(code.try_wait().unwrap().is_some());
    }

    #[tokio::test]
    async fn an_early_exit_is_an_error_while_waiting_for_a_target() {
        let mut code = tokio::process::Command::new("true").spawn().unwrap();
        code.wait().await.unwrap();
        let until = Instant::now() + Duration::from_secs(1);
        let error = wait_target(1, &mut code, until, is_workbench)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("exited early"));
    }

    #[tokio::test]
    async fn the_fake_server_lists_the_loaded_model() {
        let server = fake_server().await;
        let models = http_json(server.address().port(), "/api/v1/models")
            .await
            .unwrap();
        assert_eq!(models["models"][0]["key"], MODEL);
    }

    #[test]
    fn finds_a_tool_on_the_given_path_only() {
        let dir = env::temp_dir().join(format!("smoke-path-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("fake-tool"), "").unwrap();
        assert!(on_path("fake-tool", dir.as_os_str()));
        assert!(!on_path("fake-tool", OsStr::new("/nonexistent-dir")));
        fs::remove_dir_all(&dir).unwrap();
    }
}
