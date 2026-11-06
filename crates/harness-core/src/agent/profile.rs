//! Per-model settings, layered: built-in by name, then what LM Studio reports, then user config
//! (SPEC §5.5).

use super::recovery::ToolFormat;
use crate::provider::Capabilities;
use globset::Glob;
use serde::Deserialize;

/// One `[profiles."pattern"]` table. Unset fields leave the lower layers alone.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileLayer {
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    pub tool_format: Option<ToolFormat>,
    pub temperature: Option<f64>,
    pub stop: Option<Vec<String>>,
    pub strip_thinking: Option<bool>,
    pub constrained_retry: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelProfile {
    /// Context to load with. Not applied yet: the only source is the user layer, which M3 adds.
    pub context: Option<u32>,
    pub max_output: Option<u32>,
    pub tool_format: ToolFormat,
    pub temperature: Option<f64>,
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
            temperature: None,
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
        self.temperature = layer.temperature.or(self.temperature);
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
fn builtin_layers() -> [(&'static str, ProfileLayer); 1] {
    [(
        "*qwen2.5-coder*",
        ProfileLayer {
            tool_format: Some(ToolFormat::FencedJson),
            ..ProfileLayer::default()
        },
    )]
}

/// Resolves `model` (its key). `capabilities` is `None` if LM Studio didn't report the model.
/// `user` layers apply in order, so a later match wins, and an explicit user `tool_format`
/// beats what the server reports, with no warning.
pub fn resolve(
    model: &str,
    capabilities: Option<&Capabilities>,
    user: &[(String, ProfileLayer)],
) -> ModelProfile {
    let mut profile = ModelProfile::default();
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn caps(tools: bool) -> Capabilities {
        Capabilities {
            trained_for_tool_use: tools,
        }
    }

    fn user(pattern: &str, layer: serde_json::Value) -> (String, ProfileLayer) {
        (pattern.into(), serde_json::from_value(layer).unwrap())
    }

    #[test]
    fn an_unknown_model_with_tools_is_native_with_no_warning() {
        let got = resolve("google/gemma", Some(&caps(true)), &[]);
        assert_eq!(got.tool_format, ToolFormat::Native);
        assert_eq!(got.warning, None);
        assert!(got.constrained_retry);
    }

    #[test]
    fn unreported_capabilities_assume_nothing() {
        assert_eq!(resolve("m", None, &[]), ModelProfile::default());
    }

    #[test]
    fn a_builtin_name_sets_the_format_and_capabilities_do_not_replace_it() {
        let got = resolve("qwen/qwen2.5-coder-7b-instruct", Some(&caps(false)), &[]);
        assert_eq!(got.tool_format, ToolFormat::FencedJson);
        assert!(got.warning.is_some());
    }

    #[test]
    fn no_tool_support_means_text_calling_with_a_warning() {
        let got = resolve("m", Some(&caps(false)), &[]);
        assert_eq!(got.tool_format, ToolFormat::HermesText);
        assert!(got.warning.unwrap().contains("no tool support"));
    }

    #[test]
    fn reporting_tool_support_does_not_override_a_builtin_text_format() {
        let got = resolve("qwen2.5-coder", Some(&caps(true)), &[]);
        assert_eq!(got.tool_format, ToolFormat::FencedJson);
    }

    #[test]
    fn user_config_wins_over_every_other_layer_and_silences_the_warning() {
        let layers = [user("m*", json!({"tool_format": "native"}))];
        let got = resolve("model", Some(&caps(false)), &layers);
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
        let got = resolve("model", Some(&caps(true)), &layers);
        assert_eq!(got.context, Some(64000));
        assert_eq!(got.max_output, Some(100));
        assert_eq!(got.temperature, Some(0.2));
        assert_eq!(got.stop, ["x"]);
        assert!(!got.strip_thinking);
    }

    #[test]
    fn a_user_can_turn_constrained_retry_off() {
        let layers = [user("m", json!({"constrained_retry": false}))];
        assert!(!resolve("m", Some(&caps(true)), &layers).constrained_retry);
    }

    #[test]
    fn an_invalid_pattern_is_ignored() {
        let layers = [user("[", json!({"max_output": 1}))];
        assert_eq!(resolve("m", None, &layers), ModelProfile::default());
    }

    #[test]
    fn an_unknown_profile_field_is_rejected() {
        assert!(serde_json::from_value::<ProfileLayer>(json!({"contxt": 1})).is_err());
    }
}
