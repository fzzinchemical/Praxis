//! Per-model settings, layered: the vendor's preset, then built-in by name, then what LM Studio
//! reports, then user config (SPEC §5.5).

use super::recovery::ToolFormat;
use crate::provider::{Capabilities, Sampling, is_loopback};
use globset::Glob;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

/// One `[profiles."pattern"]` table. Unset fields leave the lower layers alone.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLayer {
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    pub tool_format: Option<ToolFormat>,
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub min_p: Option<f64>,
    pub repeat_penalty: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub strip_thinking: Option<bool>,
    pub constrained_retry: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelProfile {
    /// Context to load with when none is requested. The budget categories are shares of it.
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    pub tool_format: ToolFormat,
    pub sampling: Sampling,
    pub stop: Vec<String>,
    pub strip_thinking: bool,
    /// Retry a malformed call under its schema (SPEC §5.3). Falls back to a plain retry if the
    /// server rejects that. Off until a model is reported: support is unknown until then.
    pub constrained_retry: bool,
    /// Shown once, before the first turn.
    pub warning: Option<String>,
}

impl Default for ModelProfile {
    fn default() -> Self {
        Self {
            context: None,
            max_output: None,
            tool_format: ToolFormat::Native,
            sampling: Sampling::default(),
            stop: Vec::new(),
            strip_thinking: true,
            constrained_retry: false,
            warning: None,
        }
    }
}

impl ModelProfile {
    fn apply(&mut self, layer: &ProfileLayer) {
        self.context = layer.context.or(self.context);
        self.max_output = layer.max_output.or(self.max_output);
        self.tool_format = layer.tool_format.unwrap_or(self.tool_format);
        let sampling = &mut self.sampling;
        sampling.temperature = layer.temperature.or(sampling.temperature);
        sampling.top_p = layer.top_p.or(sampling.top_p);
        sampling.top_k = layer.top_k.or(sampling.top_k);
        sampling.min_p = layer.min_p.or(sampling.min_p);
        sampling.repeat_penalty = layer.repeat_penalty.or(sampling.repeat_penalty);
        self.strip_thinking = layer.strip_thinking.unwrap_or(self.strip_thinking);
        self.constrained_retry = layer.constrained_retry.unwrap_or(self.constrained_retry);
        if let Some(stop) = &layer.stop {
            self.stop.clone_from(stop);
        }
    }
}

fn matches(pattern: &str, model: &str) -> bool {
    match Glob::new(pattern) {
        Ok(glob) => glob.compile_matcher().is_match(model),
        Err(error) => {
            tracing::warn!(%pattern, %error, "ignoring a model profile with an invalid pattern");
            false
        }
    }
}

/// Built-ins are facts recorded in ADR 0002. Don't assume `native` from a name family.
/// Contexts are sized to fit 16 GB of VRAM, as measured in ADR 0002.
fn builtin_layers() -> [(&'static str, ProfileLayer); 3] {
    let context = |size| ProfileLayer {
        context: Some(size),
        ..ProfileLayer::default()
    };
    [
        (
            "*qwen2.5-coder*",
            ProfileLayer {
                tool_format: Some(ToolFormat::FencedJson),
                ..ProfileLayer::default()
            },
        ),
        ("*qwen3.5-4b*", context(131_072)),
        ("*gemma-4-12b-qat*", context(65_536)),
    ]
}

/// The sampling preset the model's vendor publishes in LM Studio's
/// `<home>/.lmstudio/hub/models/<key>/model.yaml`, or `None` if LM Studio isn't on this
/// machine, `key` isn't a plain `publisher/name`, or there's no readable preset.
pub fn vendor_layer(lmstudio_url: &str, home: &Path, key: &str) -> Option<ProfileLayer> {
    let local = Url::parse(lmstudio_url).is_ok_and(|url| is_loopback(&url));
    let plain = |part: &str| {
        !part.is_empty()
            && !part.starts_with('.')
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    };
    let (publisher, name) = key.split_once('/')?;
    if !local || !plain(publisher) || !plain(name) {
        return None;
    }
    let path = home
        .join(".lmstudio/hub/models")
        .join(publisher)
        .join(name)
        .join("model.yaml");
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(parse_model_yaml(&text)),
        Err(error) => {
            tracing::debug!(path = %path.display(), %error, "no vendor preset");
            None
        }
    }
}

// shortcut: reads only the `- key: llm.prediction.*` entries with a scalar `value:` or a
// `checked:`/`value:` pair, as LM Studio writes them, not full YAML; add a YAML parser if
// presets in other shapes turn up.
fn parse_model_yaml(text: &str) -> ProfileLayer {
    let mut layer = ProfileLayer::default();
    let mut field = None;
    let mut checked = true;
    for line in text.lines().map(str::trim) {
        if let Some(key) = line.strip_prefix("- key: ") {
            field = key.trim().strip_prefix("llm.prediction.");
            checked = true;
        } else if let Some(value) = line.strip_prefix("checked: ") {
            checked = value.trim() == "true";
        } else if let (Some(name), Some(value)) = (field, line.strip_prefix("value: ")) {
            if checked {
                set_vendor_field(&mut layer, name, value.trim());
            }
            field = None;
        }
    }
    layer
}

fn set_vendor_field(layer: &mut ProfileLayer, name: &str, value: &str) {
    let float = || {
        value
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    match name {
        "temperature" => layer.temperature = float(),
        "topPSampling" => layer.top_p = float(),
        "topKSampling" => layer.top_k = value.parse().ok(),
        "minPSampling" => layer.min_p = float(),
        "repeatPenalty" => layer.repeat_penalty = float(),
        _ => {}
    }
}

/// Resolves `model` (its key). `capabilities` is `None` if LM Studio didn't report the model.
/// `vendor` (from [`vendor_layer`]) sits under the built-ins, so our values win over the
/// vendor's. `user` layers apply in order, so a later match wins, and an explicit user
/// `tool_format` beats what the server reports, with no warning.
pub fn resolve(
    model: &str,
    capabilities: Option<&Capabilities>,
    vendor: Option<&ProfileLayer>,
    user: &[(String, ProfileLayer)],
) -> ModelProfile {
    let mut profile = ModelProfile::default();
    if let Some(vendor) = vendor {
        profile.apply(vendor);
    }
    for (pattern, layer) in &builtin_layers() {
        if matches(pattern, model) {
            profile.apply(layer);
        }
    }
    profile.constrained_retry = capabilities.is_some();
    let no_tools = capabilities.is_some_and(|c| !c.trained_for_tool_use);
    if no_tools && profile.tool_format == ToolFormat::Native {
        profile.tool_format = ToolFormat::HermesText;
    }
    let matching: Vec<_> = user.iter().filter(|(p, _)| matches(p, model)).collect();
    for (_, layer) in &matching {
        profile.apply(layer);
    }
    let user_chose_format = matching.iter().any(|(_, l)| l.tool_format.is_some());
    if no_tools && !user_chose_format {
        profile.warning = Some(format!(
            "`{model}` reports no tool support, so tool calls will be made through text, which is less reliable."
        ));
    }
    profile
}

/// Each field's effective value and the layer that set it, in [`resolve`]'s order, for
/// `praxis profile show`. Mirrors `resolve`: keep the two in step.
pub fn explain(
    model: &str,
    capabilities: Option<&Capabilities>,
    vendor: Option<&ProfileLayer>,
    user: &[(String, ProfileLayer)],
) -> Vec<(String, Value, String)> {
    let defaults = ProfileLayer {
        tool_format: Some(ToolFormat::Native),
        strip_thinking: Some(true),
        constrained_retry: Some(false),
        stop: Some(Vec::new()),
        ..ProfileLayer::default()
    };
    let mut layers = vec![("default".to_owned(), defaults)];
    if let Some(vendor) = vendor {
        layers.push(("vendor".to_owned(), vendor.clone()));
    }
    for (pattern, layer) in &builtin_layers() {
        if matches(pattern, model) {
            layers.push((format!("builtin \"{pattern}\""), layer.clone()));
        }
    }
    let mut so_far = ModelProfile::default();
    for (_, layer) in &layers {
        so_far.apply(layer);
    }
    let no_tools = capabilities.is_some_and(|c| !c.trained_for_tool_use);
    layers.push((
        "lmstudio".to_owned(),
        ProfileLayer {
            constrained_retry: Some(capabilities.is_some()),
            tool_format: (no_tools && so_far.tool_format == ToolFormat::Native)
                .then_some(ToolFormat::HermesText),
            ..ProfileLayer::default()
        },
    ));
    for (pattern, layer) in user {
        if matches(pattern, model) {
            layers.push((format!("user \"{pattern}\""), layer.clone()));
        }
    }

    let mut set: BTreeMap<String, (Value, String)> = BTreeMap::new();
    for (label, layer) in &layers {
        let Ok(Value::Object(fields)) = serde_json::to_value(layer) else {
            continue;
        };
        for (field, value) in fields.into_iter().filter(|(_, v)| !v.is_null()) {
            set.insert(field, (value, label.clone()));
        }
    }
    let names = serde_json::to_value(ProfileLayer::default()).unwrap_or_default();
    names
        .as_object()
        .into_iter()
        .flatten()
        .map(|(field, _)| {
            let (value, source) = set
                .get(field)
                .cloned()
                .unwrap_or_else(|| (Value::Null, "default".to_owned()));
            (field.clone(), value, source)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn caps(tools: bool) -> Capabilities {
        Capabilities {
            trained_for_tool_use: tools,
        }
    }

    fn user(pattern: &str, layer: serde_json::Value) -> (String, ProfileLayer) {
        (pattern.into(), serde_json::from_value(layer).unwrap())
    }

    #[test]
    fn context_depends_on_the_model() {
        let context = |model| resolve(model, None, None, &[]).context;
        assert_eq!(context("qwen3.5-4b"), Some(131_072));
        assert_eq!(context("google/gemma-4-12b-qat"), Some(65_536));
        assert_eq!(context("some/unknown-model"), None);
    }

    #[test]
    fn an_unknown_model_with_tools_is_native_with_no_warning() {
        let got = resolve("google/gemma", Some(&caps(true)), None, &[]);
        assert_eq!(got.tool_format, ToolFormat::Native);
        assert_eq!(got.warning, None);
        assert!(got.constrained_retry);
    }

    #[test]
    fn unreported_capabilities_assume_nothing() {
        assert_eq!(resolve("m", None, None, &[]), ModelProfile::default());
    }

    #[test]
    fn a_builtin_name_sets_the_format_and_capabilities_do_not_replace_it() {
        let got = resolve(
            "qwen/qwen2.5-coder-7b-instruct",
            Some(&caps(false)),
            None,
            &[],
        );
        assert_eq!(got.tool_format, ToolFormat::FencedJson);
        assert!(got.warning.is_some());
    }

    #[test]
    fn no_tool_support_means_text_calling_with_a_warning() {
        let got = resolve("m", Some(&caps(false)), None, &[]);
        assert_eq!(got.tool_format, ToolFormat::HermesText);
        assert!(got.warning.unwrap().contains("no tool support"));
    }

    #[test]
    fn reporting_tool_support_does_not_override_a_builtin_text_format() {
        let got = resolve("qwen2.5-coder", Some(&caps(true)), None, &[]);
        assert_eq!(got.tool_format, ToolFormat::FencedJson);
    }

    #[test]
    fn user_config_wins_over_every_other_layer_and_silences_the_warning() {
        let layers = [user("m*", json!({"tool_format": "native"}))];
        let got = resolve("model", Some(&caps(false)), None, &layers);
        assert_eq!(got.tool_format, ToolFormat::Native);
        assert_eq!(got.warning, None);
    }

    #[test]
    fn later_user_layers_override_only_the_fields_they_set() {
        let layers = [
            user(
                "m*",
                json!({"context": 64000, "max_output": 8192, "temperature": 0.2, "stop": ["x"], "strip_thinking": false}),
            ),
            user("model", json!({"max_output": 100})),
            user("other*", json!({"context": 1})),
        ];
        let got = resolve("model", Some(&caps(true)), None, &layers);
        assert_eq!(got.context, Some(64000));
        assert_eq!(got.max_output, Some(100));
        assert_eq!(got.sampling.temperature, Some(0.2));
        assert_eq!(got.stop, ["x"]);
        assert!(!got.strip_thinking);
    }

    #[test]
    fn a_user_can_turn_constrained_retry_off() {
        let layers = [user("m", json!({"constrained_retry": false}))];
        assert!(!resolve("m", Some(&caps(true)), None, &layers).constrained_retry);
    }

    const GEMMA_YAML: &str = "config:
  operation:
    fields:
      - key: llm.prediction.temperature
        value: 1.0
      - key: llm.prediction.topPSampling
        value:
          checked: true
          value: 0.95
      - key: llm.prediction.topKSampling
        value: 64
      - key: llm.prediction.repeatPenalty
        value:
          checked: false
          value: 1.1
      - key: llm.prediction.reasoning.parsing
        value:
          enabled: true
customFields:
  - key: enableThinking
    defaultValue: true
";

    #[test]
    fn a_vendor_preset_reads_only_enabled_sampling_fields() {
        let layer = parse_model_yaml(GEMMA_YAML);
        assert_eq!(layer.temperature, Some(1.0));
        assert_eq!(layer.top_p, Some(0.95));
        assert_eq!(layer.top_k, Some(64));
        assert_eq!(layer.repeat_penalty, None);
        assert_eq!(layer.min_p, None);
    }

    #[test]
    fn a_malformed_vendor_value_is_ignored() {
        let layer = parse_model_yaml(
            "- key: llm.prediction.temperature\n  value: hot\n- key: llm.prediction.topKSampling\n  value: -1\n",
        );
        assert_eq!(layer.temperature, None);
        assert_eq!(layer.top_k, None);
    }

    #[test]
    fn builtins_and_user_config_override_the_vendor_preset() {
        let vendor = parse_model_yaml(GEMMA_YAML);
        let layers = [user("*gemma*", json!({"top_k": 20}))];
        let got = resolve(
            "google/gemma-4-12b-qat",
            Some(&caps(true)),
            Some(&vendor),
            &layers,
        );
        assert_eq!(got.sampling.temperature, Some(1.0));
        assert_eq!(got.sampling.top_k, Some(20));
        assert_eq!(got.context, Some(65_536));
    }

    #[test]
    fn the_vendor_preset_is_read_from_the_local_lm_studio_hub() {
        let home = std::env::temp_dir().join(format!("praxis-vendor-{}", std::process::id()));
        let dir = home.join(".lmstudio/hub/models/google/gemma-4-12b-qat");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("model.yaml"), GEMMA_YAML).unwrap();
        let read = |url, key| vendor_layer(url, &home, key).and_then(|l| l.top_k);
        assert_eq!(
            read("http://127.0.0.1:1234", "google/gemma-4-12b-qat"),
            Some(64)
        );
        assert_eq!(read("http://gpu-box:1234", "google/gemma-4-12b-qat"), None);
        assert_eq!(
            read("http://127.0.0.1:1234", "google/../google/gemma-4-12b-qat"),
            None
        );
        assert_eq!(read("http://127.0.0.1:1234", "gemma-4-12b-qat"), None);
        assert_eq!(read("http://127.0.0.1:1234", "google/missing"), None);
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn an_invalid_pattern_is_ignored() {
        let layers = [user("[", json!({"max_output": 1}))];
        assert_eq!(resolve("m", None, None, &layers), ModelProfile::default());
    }

    fn source(rows: &[(String, Value, String)], field: &str) -> (Value, String) {
        let (_, value, source) = rows.iter().find(|(f, ..)| f.as_str() == field).unwrap();
        (value.clone(), source.clone())
    }

    #[test]
    fn explain_names_the_user_layer_that_overrode_the_vendor() {
        let vendor = ProfileLayer {
            temperature: Some(1.0),
            ..ProfileLayer::default()
        };
        let layers = [user("*gemma*", json!({"temperature": 0.5}))];
        let rows = explain(
            "google/gemma-4-12b-qat",
            Some(&caps(true)),
            Some(&vendor),
            &layers,
        );
        assert_eq!(
            source(&rows, "temperature"),
            (json!(0.5), r#"user "*gemma*""#.to_owned())
        );
        assert_eq!(
            source(&rows, "context"),
            (json!(65_536), r#"builtin "*gemma-4-12b-qat*""#.to_owned())
        );
    }

    #[test]
    fn explain_names_the_builtin_that_set_the_context() {
        let rows = explain("qwen3.5-4b", None, None, &[]);
        assert_eq!(
            source(&rows, "context"),
            (json!(131_072), r#"builtin "*qwen3.5-4b*""#.to_owned())
        );
    }

    #[test]
    fn explain_marks_unset_fields_as_default() {
        let rows = explain("m", None, None, &[]);
        assert_eq!(
            source(&rows, "max_output"),
            (Value::Null, "default".to_owned())
        );
    }

    #[test]
    fn explain_names_lmstudio_when_it_switches_the_format() {
        let rows = explain("m", Some(&caps(false)), None, &[]);
        assert_eq!(
            source(&rows, "tool_format"),
            (
                serde_json::to_value(ToolFormat::HermesText).unwrap(),
                "lmstudio".to_owned()
            )
        );
    }

    #[test]
    fn an_unknown_profile_field_is_rejected() {
        assert!(serde_json::from_value::<ProfileLayer>(json!({"contxt": 1})).is_err());
    }
}
