//! Plugin discovery and manifest (SPEC §12). Reads where each component lives; the
//! components themselves are parsed by later tasks.

use crate::config::McpServer;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};

/// A hooks or MCP source: a file to read later, or an object inlined in the manifest.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    File(PathBuf),
    Inline(Value),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plugin {
    pub name: String,
    pub root: PathBuf,
    /// Directories to scan for `<name>/SKILL.md`.
    pub skills: Vec<PathBuf>,
    /// Command and agent markdown files.
    pub commands: Vec<PathBuf>,
    pub agents: Vec<PathBuf>,
    pub hooks: Vec<Source>,
    pub mcp_servers: Vec<Source>,
    pub user_config: Option<Value>,
    pub warnings: Vec<String>,
}

const KNOWN_KEYS: &[&str] = &[
    "$schema",
    "name",
    "version",
    "description",
    "author",
    "homepage",
    "repository",
    "license",
    "keywords",
    "skills",
    "commands",
    "agents",
    "hooks",
    "mcpServers",
    "userConfig",
];
const UNSUPPORTED_KEYS: &[&str] = &[
    "lspServers",
    "outputStyles",
    "monitors",
    "themes",
    "channels",
];
const UNSUPPORTED_DEFAULTS: &[&str] = &[".lsp.json", "output-styles", "monitors", "themes"];

/// Loads the plugin at `root`. A broken manifest or a path that breaks the rules is an
/// error; anything unsupported or unknown is a named warning.
pub fn load(root: &Path) -> Result<Plugin, String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("{}: {e}", root.display()))?;
    let manifest_path = root.join(".claude-plugin/plugin.json");
    let manifest = match fs::read_to_string(&manifest_path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(Value::Object(map)) => Some(map),
            Ok(_) => return Err(format!("{}: not an object", manifest_path.display())),
            Err(e) => return Err(format!("{}: {e}", manifest_path.display())),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("{}: {e}", manifest_path.display())),
    };
    let manifest = manifest.unwrap_or_default();
    let name = match manifest.get("name") {
        Some(Value::String(name)) if !name.is_empty() => name.clone(),
        Some(_) => {
            return Err(format!(
                "{}: `name` must be a string",
                manifest_path.display()
            ));
        }
        None if manifest_path.exists() => {
            return Err(format!("{}: `name` is required", manifest_path.display()));
        }
        None => root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };

    let mut warnings = Vec::new();
    for key in manifest.keys() {
        if UNSUPPORTED_KEYS.contains(&key.as_str()) {
            warnings.push(format!(
                "plugin {name}: unsupported component `{key}` skipped"
            ));
        } else if !KNOWN_KEYS.contains(&key.as_str()) {
            warnings.push(format!(
                "plugin {name}: unknown manifest key `{key}` ignored"
            ));
        }
    }
    for default in UNSUPPORTED_DEFAULTS {
        if root.join(default).exists() {
            warnings.push(format!(
                "plugin {name}: unsupported component `{default}` skipped"
            ));
        }
    }

    let existing = |rel: &str| Some(root.join(rel)).filter(|p| p.exists());

    // `skills` adds to the default scan.
    let mut skills: Vec<PathBuf> = existing("skills").into_iter().collect();
    for entry in entries(&manifest, "skills")? {
        skills.push(component_path(&root, string(entry, "skills")?, true)?);
    }

    // `commands` and `agents` replace the default scan.
    let markdown = |key: &str| -> Result<Vec<PathBuf>, String> {
        let dirs = match manifest.get(key) {
            Some(_) => entries(&manifest, key)?
                .into_iter()
                .map(|e| component_path(&root, string(e, key)?, false))
                .collect::<Result<Vec<_>, _>>()?,
            None => existing(key).into_iter().collect(),
        };
        let mut files = Vec::new();
        for dir in dirs {
            collect_markdown(&root, &dir, &mut files)?;
        }
        Ok(files)
    };
    let commands = markdown("commands")?;
    let agents = markdown("agents")?;

    // `hooks` and `mcpServers` merge with their default files.
    let sources = |key: &str, default: &str| -> Result<Vec<Source>, String> {
        let mut found: Vec<Source> = existing(default).map(Source::File).into_iter().collect();
        for entry in entries(&manifest, key)? {
            found.push(match entry {
                Value::Object(_) => Source::Inline(entry.clone()),
                _ => Source::File(component_path(&root, string(entry, key)?, false)?),
            });
        }
        Ok(found)
    };
    let hooks = sources("hooks", "hooks/hooks.json")?;
    let mcp_servers = sources("mcpServers", ".mcp.json")?;

    Ok(Plugin {
        name,
        user_config: manifest.get("userConfig").cloned(),
        root,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        warnings,
    })
}

/// A skill or command, named `<plugin>:<name>`. Same frontmatter for both (SPEC §12).
#[derive(Debug, Clone, PartialEq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    /// With `${CLAUDE_PLUGIN_ROOT}` already expanded.
    pub body: String,
}

impl Plugin {
    /// `<dir>/<name>/SKILL.md` under each skills directory, sorted. Unreadable files are
    /// skipped with a warning.
    pub fn skills(&self) -> Vec<Skill> {
        let mut files = Vec::new();
        for dir in &self.skills {
            let mut found: Vec<PathBuf> = fs::read_dir(dir)
                .into_iter()
                .flatten()
                .flatten()
                .map(|entry| entry.path().join("SKILL.md"))
                // The same containment rule as the scan: no symlink out of the plugin.
                .filter(|p| {
                    p.canonicalize()
                        .is_ok_and(|real| real.starts_with(&self.root))
                })
                .collect();
            found.sort();
            files.extend(found);
        }
        files.iter().filter_map(|f| self.read_skill(f)).collect()
    }

    pub fn commands(&self) -> Vec<Skill> {
        self.commands
            .iter()
            .filter_map(|f| self.read_skill(f))
            .collect()
    }

    /// `agents/**/*.md` (or the manifest's `agents` paths), run as sequential subagents.
    pub fn agents(&self) -> Vec<AgentDef> {
        self.agents
            .iter()
            .filter_map(|f| {
                let (skill, field) = self.read_markdown(f)?;
                let list = |key: &str| {
                    field(key).map(|v| {
                        v.trim_matches(|c| c == '[' || c == ']')
                            .split(',')
                            .map(|t| t.trim().trim_matches(|c| c == '"' || c == '\'').to_owned())
                            .filter(|t| !t.is_empty())
                            .collect::<Vec<_>>()
                    })
                };
                Some(AgentDef {
                    tools: list("tools"),
                    disallowed_tools: list("disallowedTools").unwrap_or_default(),
                    max_turns: field("maxTurns").and_then(|v| v.parse().ok()),
                    skill,
                })
            })
            .collect()
    }

    fn read_skill(&self, path: &Path) -> Option<Skill> {
        self.read_markdown(path).map(|(skill, _)| skill)
    }

    /// The file as a skill, plus a reader for any other frontmatter field.
    fn read_markdown(&self, path: &Path) -> Option<(Skill, impl Fn(&str) -> Option<String>)> {
        let text = fs::read_to_string(path)
            .inspect_err(|error| tracing::warn!(%error, path = %path.display(), "skipped"))
            .ok()?;
        let (front, body) = split_frontmatter(&text);
        let front = front.to_owned();
        let field = move |key: &str| {
            front.lines().find_map(|line| {
                let value = line.strip_prefix(key)?.strip_prefix(':')?.trim();
                Some(value.trim_matches(|c| c == '"' || c == '\'').to_owned())
            })
        };
        // A SKILL.md is named by its directory, a command by its file.
        let stem = if path.file_name().is_some_and(|n| n == "SKILL.md") {
            path.parent()?.file_name()?
        } else {
            path.file_stem()?
        };
        let name = field("name").unwrap_or_else(|| stem.to_string_lossy().into_owned());
        let description = field("description").unwrap_or_else(|| {
            body.lines()
                .find(|l| !l.trim().is_empty())
                .unwrap_or_default()
                .trim()
                .to_owned()
        });
        let skill = Skill {
            name: format!("{}:{name}", self.name),
            description,
            body: body.replace("${CLAUDE_PLUGIN_ROOT}", &self.root.to_string_lossy()),
        };
        Some((skill, field))
    }
}

/// A plugin agent; its body is the subagent's system prompt (SPEC §12).
/// shortcut: `model` is ignored and the session model runs it; T8.6 resolves it through
/// `Config::resolve_enabled_model`.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub skill: Skill,
    /// `None` allows every tool the parent has.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
}

impl Plugin {
    /// `<data_root>/<name>`, exposed as `CLAUDE_PLUGIN_DATA`.
    fn data_dir(&self, data_root: &Path) -> PathBuf {
        data_root.join(&self.name)
    }

    /// Stores a `userConfig` value in `<data dir>/options.json`, a `0600` file, so even
    /// `sensitive` values never sit in a config file (SPEC §12).
    pub fn set_option(&self, data_root: &Path, key: &str, value: &str) -> Result<(), String> {
        if self.user_config.as_ref().and_then(|c| c.get(key)).is_none() {
            return Err(format!("plugin {}: no `userConfig` key `{key}`", self.name));
        }
        let dir = self.data_dir(data_root);
        let mut options = self.options(data_root)?;
        options.insert(key.to_owned(), value.to_owned());
        let fail = |e: std::io::Error| format!("{}: {e}", dir.display());
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(fail)?;
        let path = dir.join("options.json");
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .map_err(fail)?;
        // `mode` applies only on creation; tighten a file that already existed.
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(fail)?;
        let text = serde_json::to_string(&options).map_err(|e| e.to_string())?;
        file.write_all(text.as_bytes()).map_err(fail)
    }

    fn options(&self, data_root: &Path) -> Result<BTreeMap<String, String>, String> {
        let path = self.data_dir(data_root).join("options.json");
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    /// The plugin's MCP servers, named `plugin_<plugin>_<server>` so their tools become
    /// `mcp__plugin_<plugin>_<server>__<tool>`. `${CLAUDE_PLUGIN_ROOT}` and friends expand in
    /// `command`, `args` and `env`, and are also set in the environment, with `bin/` appended
    /// after the user's `PATH` so it can't shadow system commands (SPEC §12).
    pub fn mcp_servers(
        &self,
        project_dir: &Path,
        data_root: &Path,
    ) -> Result<BTreeMap<String, McpServer>, String> {
        let mut vars = BTreeMap::from([
            (
                "CLAUDE_PLUGIN_ROOT".to_owned(),
                self.root.display().to_string(),
            ),
            (
                "CLAUDE_PLUGIN_DATA".to_owned(),
                self.data_dir(data_root).display().to_string(),
            ),
            (
                "CLAUDE_PROJECT_DIR".to_owned(),
                project_dir.display().to_string(),
            ),
        ]);
        for (key, value) in self.options(data_root)? {
            vars.insert(
                format!("CLAUDE_PLUGIN_OPTION_{}", key.to_uppercase()),
                value,
            );
        }
        let expand = |s: &str| {
            vars.iter()
                .fold(s.to_owned(), |s, (k, v)| s.replace(&format!("${{{k}}}"), v))
        };
        let bin = self.root.join("bin");
        let mut path = std::env::var("PATH").unwrap_or_default();
        if bin.is_dir() {
            path = format!("{path}:{}", bin.display());
        }

        let mut servers = BTreeMap::new();
        for source in &self.mcp_servers {
            let value = match source {
                Source::Inline(value) => value.clone(),
                Source::File(file) => fs::read_to_string(file)
                    .map_err(|e| e.to_string())
                    .and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string()))
                    .map_err(|e| format!("{}: {e}", file.display()))?,
            };
            // With or without the `mcpServers` wrapper.
            let map = value.get("mcpServers").cloned().unwrap_or(value);
            let map: BTreeMap<String, McpServer> = serde_json::from_value(map)
                .map_err(|e| format!("plugin {}: MCP servers: {e}", self.name))?;
            for (name, mut server) in map {
                match &mut server {
                    McpServer::Stdio(server) => {
                        server.command = expand(&server.command);
                        server.args = server.args.iter().map(|a| expand(a)).collect();
                        for value in server.env.values_mut() {
                            *value = expand(value);
                        }
                        server.env.extend(vars.clone());
                        server.env.insert("PATH".to_owned(), path.clone());
                    }
                    McpServer::Http(server) => server.url = expand(&server.url),
                }
                servers.insert(format!("plugin_{}_{name}", self.name), server);
            }
        }
        Ok(servers)
    }
}

// shortcut: reads only flat `key: value` lines, not full YAML; add a YAML parser if plugins
// need multi-line values.
fn split_frontmatter(text: &str) -> (&str, &str) {
    text.strip_prefix("---\n")
        .and_then(|rest| rest.split_once("\n---\n"))
        .unwrap_or(("", text))
}

/// Expands `/<plugin>:<name> args` into the command's prompt, or `None` if `input` names no
/// command. Like Claude Code, arguments are appended when the body has no `$ARGUMENTS`.
pub fn run_command(commands: &[Skill], input: &str) -> Option<String> {
    let rest = input.strip_prefix('/')?;
    let (name, args) = rest.split_once(' ').unwrap_or((rest, ""));
    let args = args.trim();
    let command = commands.iter().find(|c| c.name == name)?;
    Some(if command.body.contains("$ARGUMENTS") {
        command.body.replace("$ARGUMENTS", args)
    } else if args.is_empty() {
        command.body.clone()
    } else {
        format!("{}\nARGUMENTS: {args}", command.body)
    })
}

/// A manifest value that may be one entry or an array of them.
fn entries<'a>(manifest: &'a Map<String, Value>, key: &str) -> Result<Vec<&'a Value>, String> {
    Ok(match manifest.get(key) {
        None => Vec::new(),
        Some(Value::Array(items)) => items.iter().collect(),
        Some(one) => vec![one],
    })
}

fn string<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .ok_or_else(|| format!("manifest `{key}`: expected a path, got {value}"))
}

/// Applies the manifest path rules: `./` prefix (`.` only for skills), no `..`, exists,
/// and resolves (through symlinks) inside the plugin root.
fn component_path(root: &Path, rel: &str, allow_root: bool) -> Result<PathBuf, String> {
    let fail = |why: &str| Err(format!("plugin path `{rel}` {why}"));
    if !(rel.starts_with("./") || (allow_root && rel == ".")) {
        return fail("must start with `./`");
    }
    if Path::new(rel)
        .components()
        .any(|c| c == Component::ParentDir)
    {
        return fail("must not contain `..`");
    }
    let path = root.join(rel);
    let Ok(real) = path.canonicalize() else {
        return fail("does not exist");
    };
    if !real.starts_with(root) {
        return fail("resolves outside the plugin root");
    }
    Ok(path)
}

fn collect_markdown(root: &Path, path: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    // A symlink inside a scanned directory must not lead out of the plugin either.
    if !path.canonicalize().is_ok_and(|real| real.starts_with(root)) {
        return Err(format!(
            "{} resolves outside the plugin root",
            path.display()
        ));
    }
    if !path.is_dir() {
        out.push(path.to_path_buf());
        return Ok(());
    }
    let mut children = fs::read_dir(path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    children.sort();
    for child in children {
        if child.is_dir() || child.extension().is_some_and(|ext| ext == "md") {
            collect_markdown(root, &child, out)?;
        }
    }
    Ok(())
}

impl Plugin {
    /// The plugin's command hooks: files wrap the event map in `"hooks"`, inline objects don't.
    pub fn hooks(&self) -> Result<(Vec<crate::hooks::Hook>, Vec<String>), String> {
        let (mut hooks, mut warnings) = (Vec::new(), Vec::new());
        for source in &self.hooks {
            let events = match source {
                Source::Inline(value) => value.clone(),
                Source::File(file) => fs::read_to_string(file)
                    .map_err(|e| e.to_string())
                    .and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string()))
                    .map_err(|e| format!("{}: {e}", file.display()))?
                    .get("hooks")
                    .cloned()
                    .unwrap_or(Value::Null),
            };
            let (found, skipped) = crate::hooks::parse(&events);
            hooks.extend(found);
            warnings.extend(skipped);
        }
        Ok((hooks, warnings))
    }

    /// Everything the plugin can run, one per line, shown before it is enabled (SPEC §10.9).
    pub fn runs(&self, data_root: &Path) -> Result<Vec<String>, String> {
        let (hooks, warnings) = self.hooks()?;
        let mut lines: Vec<String> = hooks
            .iter()
            .map(|h| {
                let args = h.args.as_ref().map(|a| a.join(" ")).unwrap_or_default();
                format!("hook {} {:?}: {} {args}", h.event, h.matcher, h.command)
            })
            .collect();
        for (name, server) in self.mcp_servers(&self.root, data_root)? {
            lines.push(match server {
                McpServer::Stdio(s) => {
                    format!("MCP server {name}: {} {}", s.command, s.args.join(" "))
                }
                McpServer::Http(s) => format!("MCP server {name}: {}", s.url),
            });
        }
        if let Ok(entries) = fs::read_dir(self.root.join("bin")) {
            for entry in entries.flatten() {
                lines.push(format!("bin/{}", entry.file_name().to_string_lossy()));
            }
        }
        let warnings = warnings.into_iter().chain(self.warnings.iter().cloned());
        lines.extend(warnings.map(|w| format!("warning: {w}")));
        Ok(lines)
    }
}

/// One entry in `installed.json`.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Installed {
    pub source: String,
    pub commit: String,
    pub enabled: bool,
}

/// Installed plugins under `<root>/<name>/`, their data under `<root>/data/<name>/`.
pub struct Store {
    pub root: PathBuf,
}

impl Store {
    pub fn data_root(&self) -> PathBuf {
        self.root.join("data")
    }

    pub fn list(&self) -> Result<BTreeMap<String, Installed>, String> {
        let path = self.root.join("installed.json");
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    fn save(&self, list: &BTreeMap<String, Installed>) -> Result<(), String> {
        let path = self.root.join("installed.json");
        let text = serde_json::to_string_pretty(list).map_err(|e| e.to_string())?;
        fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn get(&self, name: &str) -> Result<(Installed, Plugin), String> {
        let entry = self
            .list()?
            .remove(name)
            .ok_or_else(|| format!("no plugin `{name}` is installed"))?;
        Ok((entry, load(&self.root.join(name))?))
    }

    /// Clones a git URL or local repo and pins its HEAD commit. Installs disabled.
    // shortcut: pins the default branch's HEAD only; add `--rev` when someone needs another.
    pub fn install(&self, source: &str) -> Result<(Installed, Plugin), String> {
        let staging = self.root.join(".install");
        let _ = fs::remove_dir_all(&staging);
        fs::create_dir_all(&staging).map_err(|e| format!("{}: {e}", staging.display()))?;
        // A plugin without a manifest is named after its directory, so keep the source's.
        let base = source
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("plugin");
        let clone = staging.join(base.strip_suffix(".git").unwrap_or(base));
        let target = clone.to_string_lossy();
        git(&staging, &["clone", "--quiet", "--", source, &target])?;
        let commit = git(&clone, &["rev-parse", "HEAD"])?;
        let name = load(&clone)?.name;
        let valid = name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !valid || name == "data" {
            return Err(format!(
                "plugin name `{name}` must be letters, digits, `-` or `_`, and not `data`"
            ));
        }
        let mut list = self.list()?;
        if list.contains_key(&name) {
            return Err(format!(
                "plugin `{name}` is already installed; remove it first"
            ));
        }
        let dest = self.root.join(&name);
        fs::rename(&clone, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
        let _ = fs::remove_dir_all(&staging);
        let entry = Installed {
            source: source.to_owned(),
            commit,
            enabled: false,
        };
        list.insert(name, entry.clone());
        self.save(&list)?;
        Ok((entry, load(&dest)?))
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<Plugin, String> {
        let (_, plugin) = self.get(name)?;
        let mut list = self.list()?;
        if let Some(entry) = list.get_mut(name) {
            entry.enabled = enabled;
        }
        self.save(&list)?;
        Ok(plugin)
    }

    /// Removes the plugin but not its data directory, which needs its own confirmation.
    pub fn remove(&self, name: &str) -> Result<(), String> {
        let mut list = self.list()?;
        if list.remove(name).is_none() {
            return Err(format!("no plugin `{name}` is installed"));
        }
        let dir = self.root.join(name);
        fs::remove_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        self.save(&list)
    }
}

fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        // Inherited from a git hook, these would aim the clone at some other repo.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("git {}: {}", args.join(" "), stderr.trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}
