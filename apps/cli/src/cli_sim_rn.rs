//! `oximux sim net` and `oximux sim rn` arguments (v32). Kept beside
//! `cli.rs` so that file stays small.

use clap::{Args, Subcommand};

#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum NetCommand {
    /// An Android emulator's network speed: gsm, hscsd, gprs, edge, umts,
    /// hsdpa, lte, evdo, full, or kbit/s. The emulator shapes only mobile
    /// data, so a speed below full turns its Wi-Fi off (full turns it on).
    Speed { speed: String },
    /// Turn off the device's Wi-Fi and mobile data (Android). `net online`
    /// turns them back on.
    Offline,
    /// Turn the device's Wi-Fi and mobile data back on (Android).
    Online,
}

#[derive(Subcommand, Clone, Debug, PartialEq, Eq)]
pub enum RnCommand {
    /// The React Native app's HTTP requests while it runs for a few seconds:
    /// method, status, time, size and URL (React Native 0.83 or later, in
    /// a debug build served by Metro on port 8081–8089).
    Network(RnNetworkArgs),
    /// Where the app's JavaScript spends its time: a CPU profile for a few
    /// seconds, its functions by self time.
    Profile(RnProfileArgs),
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct RnNetworkArgs {
    /// How long to watch (1–60).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub seconds: u32,
    /// Only requests whose URL contains this.
    #[arg(long, value_name = "TEXT")]
    pub grep: Option<String>,
    /// Also each request's headers and text bodies (at most 4 KiB each);
    /// secrets in them are redacted.
    #[arg(long)]
    pub bodies: bool,
    /// The app's bundle id or package, when several are connected to Metro.
    #[arg(long, value_name = "ID")]
    pub app: Option<String>,
}

#[derive(Args, Clone, Debug, PartialEq, Eq)]
pub struct RnProfileArgs {
    /// How long to profile (1–60).
    #[arg(long, default_value_t = 10, value_parser = clap::value_parser!(u32).range(1..=60))]
    pub seconds: u32,
    /// How many functions to list (1–100).
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=100))]
    pub top: u32,
    /// The app's bundle id or package, when several are connected to Metro.
    #[arg(long, value_name = "ID")]
    pub app: Option<String>,
}
