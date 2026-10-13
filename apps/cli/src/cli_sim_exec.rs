//! `oximux sim exec` arguments (v31). Kept beside `cli.rs` so that file stays
//! small.

use clap::Args;

#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct ExecArgs {
    /// The command after `simctl` (iOS simulator) or `adb` (Android), after
    /// `--`, without the device: OxiMux adds the attached one.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true, value_name = "ARGS")]
    pub argv: Vec<String>,
}
