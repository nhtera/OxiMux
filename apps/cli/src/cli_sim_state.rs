//! `oximux sim`'s device-state arguments (v28): `location`, `appearance`,
//! `privacy`, `push`, `status-bar`, `erase`.

use std::path::PathBuf;

use clap::{Args, Subcommand, ValueEnum};

#[derive(Subcommand, Clone, Debug, PartialEq)]
pub enum LocationCommand {
    /// Pretend to be at LAT LON (degrees; south and west are negative).
    Set {
        #[arg(allow_negative_numbers = true)]
        lat: f64,
        #[arg(allow_negative_numbers = true)]
        lon: f64,
    },
    /// Stop pretending (a simulator only; an emulator keeps its last place).
    Clear,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppearanceArg {
    Light,
    Dark,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyActionArg {
    Grant,
    Revoke,
    /// Back to "not asked yet", for that app only.
    Reset,
}

#[derive(Args, Clone, Debug, PartialEq)]
pub struct PrivacyArgs {
    #[arg(value_enum)]
    pub action: PrivacyActionArg,
    /// calendar, contacts, location, location-always, photos, photos-add,
    /// media-library, microphone, motion, reminders, siri, camera,
    /// notifications (what each platform has differs).
    pub service: String,
    /// The app's bundle id (Android: package name). Required: nothing
    /// changes for every app at once.
    #[arg(long, value_name = "ID", required = true)]
    pub app: String,
}

#[derive(Args, Clone, Debug, PartialEq)]
pub struct PushArgs {
    /// The app's bundle id.
    #[arg(long, value_name = "ID")]
    pub app: String,
    /// The notification's JSON: an object with `aps` (at most 4096 bytes).
    pub file: PathBuf,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusBarArg {
    /// 9:41, full battery and signal: for screenshots.
    Clean,
    /// The real status bar back.
    Clear,
}
