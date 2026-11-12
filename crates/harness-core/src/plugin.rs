//! Plugin discovery and manifest (SPEC §12). Reads where each component lives; the
//! components themselves are parsed by later tasks.

use crate::config::{Config, McpServer, ModelRef};
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

    let plugin = Plugin {
        name,
        user_config: manifest.get("userConfig").cloned(),
        root,
        skills,
        commands,
        agents,
        hooks,
        mcp_servers,
        warnings,
    };
    // `general` is the built-in agent's name (SPEC §12); an agent file named so is an error.
    let reserved = format!("{}:general", plugin.name);
    if plugin
        .agents
        .iter()
        .filter_map(|f| plugin.read_markdown(f))
        .any(|(skill, _)| skill.name == reserved)
    {
        return Err(format!(
            "{}: agent `general` is reserved for the built-in agent",
            plugin.name
        ));
    }
    Ok(plugin)
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
        let files: Vec<PathBuf> = self
            .skills
            .iter()
            .flat_map(|dir| skill_files(dir, &self.root))
            .collect();
        files.iter().filter_map(|f| self.read_skill(f)).collect()
    }

    pub fn commands(&self) -> Vec<Skill> {
        self.commands
            .iter()
            .filter_map(|f| self.read_skill(f))
            .collect()
    }

    /// `agents/**/*.md` (or the manifest's `agents` paths), run as sequential subagents.
    /// An agent whose `model` doesn't resolve is skipped with a warning, so it never runs on
    /// a model the user didn't enable.
    pub fn agents(&self, config: &Config) -> Vec<AgentDef> {
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
                let model = match field("model") {
                    None => None,
                    Some(reference) => match config.resolve_enabled_model(&reference) {
                        Ok(model) => Some(model),
                        Err(error) => {
                            tracing::warn!(%error, path = %f.display(), "agent skipped");
                            return None;
                        }
                    },
                };
                Some(AgentDef {
                    tools: list("tools"),
                    disallowed_tools: list("disallowedTools").unwrap_or_default(),
                    max_turns: field("maxTurns").and_then(|v| v.parse().ok()),
                    model,
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
        let (mut skill, field) = parse_markdown(path, default_stem(path)?)?;
        skill.name = format!("{}:{}", self.name, skill.name);
        skill.body = skill
            .body
            .replace("${CLAUDE_PLUGIN_ROOT}", &self.root.to_string_lossy());
        Some((skill, field))
    }
}

/// Frontmatter that would grant permissions or start processes. Markdown can't grant
/// anything, so these are ignored with a warning (SPEC §12).
const IGNORED_KEYS: &[&str] = &["allowed-tools", "permissionMode", "hooks", "mcpServers"];

/// Parses a skill, command or agent file. `stem` names it when it has no `name` field. The
/// body is returned as written; substitutions are the caller's business.
fn parse_markdown(path: &Path, stem: String) -> Option<(Skill, impl Fn(&str) -> Option<String>)> {
    let text = fs::read_to_string(path)
        .inspect_err(|error| tracing::warn!(%error, path = %path.display(), "skipped"))
        .ok()?;
    let (front, body) = split_frontmatter(&text);
    let front = front.to_owned();
    let field = move |key: &str| {
        let mut lines = front.lines();
        let value = lines
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(':'))?
            .trim();
        // A `>` or `|` block scalar continues on indented lines; the index wants one line.
        if value.starts_with(['>', '|']) {
            let folded: Vec<&str> = lines
                .take_while(|l| l.trim().is_empty() || l.starts_with([' ', '\t']))
                .map(str::trim)
                .filter(|l| !l.is_empty())
                .collect();
            return Some(folded.join(" "));
        }
        Some(value.trim_matches(|c| c == '"' || c == '\'').to_owned())
    };
    for key in IGNORED_KEYS.iter().filter(|key| field(key).is_some()) {
        tracing::warn!(key, path = %path.display(), "ignored: markdown can't grant permissions");
    }
    // shortcut: any `!` backtick pair warns, even in prose; not parsed as injection because it never runs.
    if body.contains("!`") {
        tracing::warn!(path = %path.display(), "ignored: shell injection never runs from markdown");
    }
    let name = field("name").unwrap_or(stem);
    let description = field("description").unwrap_or_else(|| {
        body.lines()
            .find(|l| !l.trim().is_empty())
            .unwrap_or_default()
            .trim()
            .to_owned()
    });
    let skill = Skill {
        name,
        description,
        body: body.to_owned(),
    };
    Some((skill, field))
}

/// A SKILL.md is named by its directory, a command by its file.
fn default_stem(path: &Path) -> Option<String> {
    let stem = if path.file_name().is_some_and(|n| n == "SKILL.md") {
        path.parent()?.file_name()?
    } else {
        path.file_stem()?
    };
    Some(stem.to_string_lossy().into_owned())
}

/// `<dir>/<name>/SKILL.md` for each subdirectory, sorted. A file that resolves outside `root`
/// is not read: no symlink may lead out of its plugin or layer.
fn skill_files(dir: &Path, root: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("SKILL.md"))
        .filter(|p| p.canonicalize().is_ok_and(|real| real.starts_with(root)))
        .collect();
    found.sort();
    found
}

/// The `.md` files under `dir`, sorted. Unlike the plugin scan, a file that resolves outside
/// `root` is skipped with a warning rather than failing the whole directory.
fn command_files(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if !child
            .canonicalize()
            .is_ok_and(|real| real.starts_with(root))
        {
            tracing::warn!(path = %child.display(), "skipped: resolves outside its layer");
        } else if child.is_dir() {
            command_files(&child, root, out);
        } else if child.extension().is_some_and(|ext| ext == "md") {
            out.push(child);
        }
    }
}

/// Standalone skills and commands (SPEC §12), from `skills/` and `commands/` under each layer
/// root, highest precedence first.
#[derive(Debug, Default, PartialEq)]
pub struct Standalone {
    pub skills: Vec<Skill>,
    /// Subdirectories join with `:`, so `frontend/component.md` is `frontend:component`.
    pub commands: Vec<Skill>,
}

/// Reads every layer. A skill beats a command of the same name; within each kind the first
/// layer wins. A file that resolves outside its layer is skipped.
pub fn standalone(layers: &[PathBuf]) -> Standalone {
    let roots: Vec<(PathBuf, &PathBuf)> = layers
        .iter()
        .filter_map(|layer| Some((layer.canonicalize().ok()?, layer)))
        .collect();
    let mut out = Standalone::default();
    for (root, layer) in &roots {
        for file in skill_files(&layer.join("skills"), root) {
            let stem = default_stem(&file).unwrap_or_default();
            if let Some((skill, _)) = parse_markdown(&file, stem)
                && !out.skills.iter().any(|s| s.name == skill.name)
            {
                out.skills.push(skill);
            }
        }
    }
    for (root, layer) in &roots {
        let dir = layer.join("commands");
        let mut files = Vec::new();
        command_files(&dir, root, &mut files);
        for file in files {
            let Ok(rel) = file.strip_prefix(&dir) else {
                continue;
            };
            let stem = rel
                .with_extension("")
                .components()
                .map(|c| c.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join(":");
            if let Some((command, _)) = parse_markdown(&file, stem) {
                let taken = out
                    .skills
                    .iter()
                    .chain(&out.commands)
                    .any(|s| s.name == command.name);
                if !taken {
                    out.commands.push(command);
                }
            }
        }
    }
    out
}

/// A plugin agent; its body is the subagent's system prompt (SPEC §12).
/// `model` is resolved by `Config::resolve_enabled_model`; `None` runs on the session model.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentDef {
    pub skill: Skill,
    /// `None` allows every tool the parent has.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Vec<String>,
    pub max_turns: Option<u32>,
    pub model: Option<ModelRef>,
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

// shortcut: reads flat `key: value` lines and `>`/`|` block scalars (folded to one line), not
// full YAML; add a YAML parser if plugins need lists or nested maps.
pub(crate) fn split_frontmatter(text: &str) -> (&str, &str) {
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

pub(crate) fn collect_markdown(
    root: &Path,
    path: &Path,
    out: &mut Vec<PathBuf>,
) -> Result<(), String> {
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
            let events =
                match source {
                    Source::Inline(value) => value.clone(),
                    Source::File(file) => {
                        let mut root = fs::read_to_string(file)
                            .map_err(|e| e.to_string())
                            .and_then(|t| {
                                serde_json::from_str::<Map<String, Value>>(&t)
                                    .map_err(|e| e.to_string())
                            })
                            .map_err(|e| format!("{}: {e}", file.display()))?;
                        let events = root.remove("hooks").unwrap_or(Value::Null);
                        warnings.extend(root.keys().map(|key| {
                            format!("{}: unsupported key `{key}` ignored", file.display())
                        }));
                        events
                    }
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
        let checkout = staging.join(base.strip_suffix(".git").unwrap_or(base));
        // A local plugin may sit in a subdirectory of a marketplace repo: clone the repo, keep the subdirectory.
        // shortcut: git URLs must point at a plugin repo's root; add a subdirectory argument when someone needs it.
        let (repo, prefix) = match Path::new(source).is_dir() {
            true => (
                git(Path::new(source), &["rev-parse", "--show-toplevel"])?,
                git(Path::new(source), &["rev-parse", "--show-prefix"])?,
            ),
            false => (source.to_owned(), String::new()),
        };
        let target = checkout.to_string_lossy();
        git(&staging, &["clone", "--quiet", "--", &repo, &target])?;
        let commit = git(&checkout, &["rev-parse", "HEAD"])?;
        let clone = checkout.join(&prefix);
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
