//! The model profile popover's pure logic (SPEC §11). The extension shells out to `praxis profile`,
//! so this module only builds argv, maps `profile show --json` rows and builds the panel message.

use serde_json::{Value, json};

/// The fields a profile can set, in the order `praxis profile show` lists them.
pub const FIELDS: [&str; 11] = [
    "context",
    "max_output",
    "tool_format",
    "temperature",
    "top_p",
    "top_k",
    "min_p",
    "repeat_penalty",
    "stop",
    "strip_thinking",
    "constrained_retry",
];

/// One profile field, as `praxis profile show --json` prints it.
#[derive(Debug, PartialEq)]
pub struct Row {
    pub field: String,
    /// `None` when the field is unset everywhere.
    pub value: Option<Value>,
    /// Where the value comes from, as the CLI names it.
    pub source: String,
}

pub fn show_args(key: &str) -> Vec<String> {
    ["profile", "show", key, "--json"]
        .map(str::to_owned)
        .to_vec()
}

/// `--` keeps a dash-leading value such as `-1` from being read as a flag.
pub fn set_args(key: &str, field: &str, value: &str) -> Vec<String> {
    ["--model", key, "profile", "set", "--", field, value]
        .map(str::to_owned)
        .to_vec()
}

/// Maps a `profile show --json` answer. One malformed row fails the whole listing.
pub fn parse_rows(json: &str) -> Result<Vec<Row>, String> {
    let rows: Vec<Value> = serde_json::from_str(json).map_err(|error| error.to_string())?;
    rows.iter().map(parse_row).collect()
}

fn parse_row(row: &Value) -> Result<Row, String> {
    let text = |key: &str| {
        row[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("profile row has no `{key}`"))
    };
    Ok(Row {
        field: text("field")?,
        value: Some(row["value"].clone()).filter(|value| !value.is_null()),
        source: text("source")?,
    })
}

/// The panel message that fills the popover for `key`.
pub fn rows_message(key: &str, rows: &[Row]) -> Value {
    let rows: Vec<Value> = rows
        .iter()
        .map(|row| json!({"field": row.field, "value": row.value, "source": row.source}))
        .collect();
    json!({"type": "profile", "model": key, "rows": rows})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_names_the_model_and_asks_for_json() {
        assert_eq!(show_args("qwen"), ["profile", "show", "qwen", "--json"]);
    }

    #[test]
    fn set_passes_the_separator_before_field_and_value() {
        assert_eq!(
            set_args("qwen", "temperature", "-0.5"),
            [
                "--model",
                "qwen",
                "profile",
                "set",
                "--",
                "temperature",
                "-0.5"
            ]
        );
    }

    #[test]
    fn rows_map_unset_values_to_none_and_keep_the_source() {
        let rows = parse_rows(
            r#"[{"field":"context","value":4096,"source":"config"},
                {"field":"stop","value":null,"source":"unset"}]"#,
        )
        .unwrap();
        assert_eq!(rows[0].value, Some(json!(4096)));
        assert_eq!(rows[0].source, "config");
        assert_eq!(
            rows[1],
            Row {
                field: "stop".into(),
                value: None,
                source: "unset".into()
            }
        );
    }

    #[test]
    fn a_malformed_row_fails_the_whole_listing() {
        assert!(parse_rows(r#"[{"field":"context","source":"x"},{"value":1}]"#).is_err());
        assert!(parse_rows("not json").is_err());
    }

    #[test]
    fn the_panel_message_carries_model_and_rows() {
        let rows = [Row {
            field: "top_k".into(),
            value: None,
            source: "unset".into(),
        }];
        assert_eq!(
            rows_message("qwen", &rows),
            json!({"type": "profile", "model": "qwen",
                   "rows": [{"field": "top_k", "value": null, "source": "unset"}]})
        );
    }
}
