//! Plugin discovery and manifest (SPEC §12). Reads where each component lives; the
//! components themselves are parsed by later tasks.

use serde_json::{Map, Value};
use std::fs;
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

    fn read_skill(&self, path: &Path) -> Option<Skill> {
        let text = fs::read_to_string(path)
            .inspect_err(|error| tracing::warn!(%error, path = %path.display(), "skipped"))
            .ok()?;
        let (front, body) = split_frontmatter(&text);
        let field = |key: &str| {
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
        Some(Skill {
            name: format!("{}:{name}", self.name),
            description,
            body: body.replace("${CLAUDE_PLUGIN_ROOT}", &self.root.to_string_lossy()),
        })
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
