//! Dev tasks. `cargo xtask vsix` packages the VS Code extension without npm or vsce (ADR 0001);
//! `cargo xtask eval` runs the model evals (SPEC §14); `vscode-smoke` drives a real VS Code (SPEC §14).

mod eval;
mod smoke;

use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::{env, fs};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const WASM_TARGET: &str = "wasm32-unknown-unknown";
const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
<Default Extension="vsixmanifest" ContentType="text/xml"/>
<Default Extension="json" ContentType="application/json"/>
<Default Extension="js" ContentType="application/javascript"/>
<Default Extension="wasm" ContentType="application/wasm"/>
<Default Extension="svg" ContentType="image/svg+xml"/>
<Default Extension="css" ContentType="text/css"/>
</Types>
"#;

fn main() -> Result<()> {
    // Before the runtime starts, so the sandbox applies to a single-threaded process (SPEC §9).
    let args: Vec<std::ffi::OsString> = env::args_os().skip(1).collect();
    if let Some((command, rest)) = args.split_first()
        && command == "sandbox-exec"
    {
        bail!("{}", praxis_core::sandbox::exec(rest));
    }
    async_main()
}

#[tokio::main]
async fn async_main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.split_first() {
        Some((command, _)) if command == "vsix" => vsix(),
        Some((command, _)) if command == "vscode-smoke" => smoke::vscode_smoke().await,
        Some((command, rest)) if command == "eval" => eval::eval(rest, workspace_root()?).await,
        // Runs inside the sandbox that `eval` starts.
        Some((command, [spec])) if command == "eval-agent" => eval::agent_main(spec).await,
        _ => bail!("usage: cargo xtask vsix | vscode-smoke | eval --model <id>"),
    }
}

pub(crate) fn workspace_root() -> Result<&'static Path> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .context("xtask has no parent directory")
}

pub(crate) fn vsix() -> Result<()> {
    let root = workspace_root()?;
    let out = root.join("target/vsix");
    // Stale glue from an earlier build must not ship.
    if out.exists() {
        fs::remove_dir_all(&out)?;
    }
    build_wasm(root, "praxis-vscode", "nodejs", &out.join("dist"))?;
    // `deno` is the only target that fetches and starts the wasm on import: `web` needs an init call the
    // CSP bars, and Electron rejects the `import source` syntax of `module` (ADR 0001).
    build_wasm(root, "praxis-vscode-ui", "deno", &out.join("ui"))?;
    fs::copy(root.join("editors/vscode/ui.css"), out.join("ui/ui.css"))?;
    fs::create_dir_all(out.join("media"))?;
    fs::copy(
        root.join("editors/vscode/media/praxis.svg"),
        out.join("media/praxis.svg"),
    )?;

    let package = fs::read_to_string(root.join("editors/vscode/package.json"))?;
    let manifest = vsix_manifest(&serde_json::from_str(&package)?)?;
    let mut entries = vec![
        (
            "[Content_Types].xml".to_owned(),
            CONTENT_TYPES.as_bytes().to_vec(),
        ),
        ("extension.vsixmanifest".to_owned(), manifest.into_bytes()),
        ("extension/package.json".to_owned(), package.into_bytes()),
    ];
    for dir in ["dist", "ui", "media"] {
        for file in fs::read_dir(out.join(dir))? {
            let path = file?.path();
            let name = path.file_name().context("no file name")?.to_string_lossy();
            entries.push((format!("extension/{dir}/{name}"), fs::read(&path)?));
        }
    }

    let path = root.join("target/praxis.vsix");
    write_zip(&path, &entries)?;
    eprintln!("wrote {}", path.display());
    Ok(())
}

fn build_wasm(root: &Path, krate: &str, target: &str, out: &Path) -> Result<()> {
    let profile = "wasm-release";
    run(Command::new("cargo").current_dir(root).args([
        "build",
        "--profile",
        profile,
        "--target",
        WASM_TARGET,
        "-p",
        krate,
    ]))?;
    let wasm = root
        .join("target")
        .join(WASM_TARGET)
        .join(profile)
        .join(format!("{}.wasm", krate.replace('-', "_")));
    run(Command::new("wasm-bindgen")
        .args(["--no-typescript", "--target", target, "--out-dir"])
        .arg(out)
        .arg(wasm))
}

pub(crate) fn run(command: &mut Command) -> Result<()> {
    let status = command
        .status()
        .with_context(|| format!("failed to start {:?}", command.get_program()))?;
    if !status.success() {
        bail!("{command:?} failed: {status}");
    }
    Ok(())
}

/// Identity must match package.json, or VS Code rejects the package.
fn vsix_manifest(package: &Value) -> Result<String> {
    let field = |name: &str| {
        package[name]
            .as_str()
            .with_context(|| format!("package.json has no string `{name}`"))
    };
    let (id, version, publisher) = (field("name")?, field("version")?, field("publisher")?);
    let display = field("displayName")?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<PackageManifest Version="2.0.0" xmlns="http://schemas.microsoft.com/developer/vsx-schema/2011">
<Metadata>
<Identity Language="en-US" Id="{id}" Version="{version}" Publisher="{publisher}"/>
<DisplayName>{display}</DisplayName>
</Metadata>
<Installation><InstallationTarget Id="Microsoft.VisualStudio.Code"/></Installation>
<Dependencies/>
<Assets>
<Asset Type="Microsoft.VisualStudio.Code.Manifest" Path="extension/package.json" Addressable="true"/>
</Assets>
</PackageManifest>
"#
    ))
}

fn write_zip(path: &Path, entries: &[(String, Vec<u8>)]) -> Result<()> {
    let mut zip = ZipWriter::new(fs::File::create(path)?);
    for (name, bytes) in entries {
        zip.start_file(name.as_str(), SimpleFileOptions::default())?;
        zip.write_all(bytes)?;
    }
    zip.finish()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_identity_comes_from_package_json() {
        let package = serde_json::json!({
            "name": "n", "version": "1.2.3", "publisher": "p", "displayName": "d"
        });
        let manifest = vsix_manifest(&package).unwrap();
        assert!(manifest.contains(r#"Id="n" Version="1.2.3" Publisher="p""#));
    }

    #[test]
    fn manifest_rejects_a_package_without_a_publisher() {
        let package = serde_json::json!({ "name": "n", "version": "1", "displayName": "d" });
        assert!(vsix_manifest(&package).is_err());
    }
}
