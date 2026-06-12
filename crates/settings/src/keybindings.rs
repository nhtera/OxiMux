//! `keybindings.toml` round-trip — the user's keyboard-shortcut overrides.
//!
//! The file is a flat table of `action_id = "chord"` entries; an empty
//! string unbinds the action. This module only parses and serializes the
//! raw map: action-id and chord validation need the action inventory and
//! the platform keystroke parser, both of which live in the app crate.

use std::collections::BTreeMap;

/// Parsed contents of `keybindings.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KeybindingOverrides {
    /// `action_id -> chord` ("" = explicitly unbound).
    pub overrides: BTreeMap<String, String>,
    /// Keys whose values were not strings (e.g. `new_tab = 3`). Kept so the
    /// app can warn instead of silently dropping a malformed line.
    pub non_string_keys: Vec<String>,
}

impl KeybindingOverrides {
    pub const FILE_NAME: &'static str = "keybindings.toml";

    /// Parse the override table. A syntactically invalid file is a hard
    /// error (the caller falls back to defaults + warns); a well-formed
    /// file with non-string values keeps the good entries and reports the
    /// bad keys.
    pub fn from_toml_str(text: &str) -> Result<Self, toml::de::Error> {
        let table: toml::Table = toml::from_str(text)?;
        let mut overrides = BTreeMap::new();
        let mut non_string_keys = Vec::new();
        for (key, value) in table {
            match value {
                toml::Value::String(chord) => {
                    overrides.insert(key, chord);
                }
                _ => non_string_keys.push(key),
            }
        }
        Ok(Self {
            overrides,
            non_string_keys,
        })
    }

    /// Serialize `overrides` with a self-documenting header. Sorted (BTreeMap
    /// order) so repeated saves produce stable diffs.
    pub fn to_toml_string(overrides: &BTreeMap<String, String>) -> String {
        let mut out = String::from(
            "# OxiMux keyboard-shortcut overrides.\n\
             # One `action_id = \"chord\"` per line; action ids are shown in\n\
             # Settings -> Keybindings. Chords use gpui syntax, e.g. \"cmd-shift-t\"\n\
             # or multi-stroke \"cmd-k cmd-b\". An empty string unbinds the action.\n\n",
        );
        for (key, chord) in overrides {
            let key = if key
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                && !key.is_empty()
            {
                key.clone()
            } else {
                toml::Value::String(key.clone()).to_string()
            };
            let value = toml::Value::String(chord.clone()).to_string();
            out.push_str(&format!("{key} = {value}\n"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_flat_string_table() {
        let parsed = KeybindingOverrides::from_toml_str("new_tab = \"cmd-y\"\nsearch = \"\"\n")
            .expect("valid toml");
        assert_eq!(parsed.overrides.get("new_tab").unwrap(), "cmd-y");
        assert_eq!(parsed.overrides.get("search").unwrap(), "");
        assert!(parsed.non_string_keys.is_empty());
    }

    #[test]
    fn non_string_values_are_reported_not_fatal() {
        let parsed = KeybindingOverrides::from_toml_str("good = \"cmd-y\"\nbad = 3\n")
            .expect("valid toml");
        assert_eq!(parsed.overrides.len(), 1);
        assert_eq!(parsed.non_string_keys, vec!["bad".to_string()]);
    }

    #[test]
    fn invalid_toml_is_an_error() {
        assert!(KeybindingOverrides::from_toml_str("not toml at all [").is_err());
    }

    #[test]
    fn round_trips_through_save_format() {
        let mut map = BTreeMap::new();
        map.insert("new_tab".to_string(), "cmd-y".to_string());
        map.insert("dismiss_overlay".to_string(), String::new());
        let text = KeybindingOverrides::to_toml_string(&map);
        let parsed = KeybindingOverrides::from_toml_str(&text).expect("round-trip parses");
        assert_eq!(parsed.overrides, map);
    }
}
