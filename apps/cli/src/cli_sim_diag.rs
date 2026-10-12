//! `oximux sim`'s diagnostics arguments (v29): `logs`, `crashes`.

use clap::{Args, ValueEnum};

/// The furthest back either verb reads (the host refuses more too).
pub const MAX_SINCE_SECS: u32 = 3600;

#[derive(Args, Clone, Debug, PartialEq)]
pub struct LogsArgs {
    /// How far back: 30s, 5m, 1h (at most 1h).
    #[arg(long, value_name = "TIME", default_value = "30s", value_parser = parse_since)]
    pub since: u32,
    /// One app's messages (bundle id; Android: package name). Required on a
    /// real Android phone; on a simulator, without it (or --grep) at most
    /// 10m (5m with info or debug).
    #[arg(long, value_name = "ID")]
    pub app: Option<String>,
    /// Only messages containing TEXT (ignoring case).
    #[arg(long, value_name = "TEXT")]
    pub grep: Option<String>,
    /// error: errors and faults only; info and debug add more.
    #[arg(long, value_enum)]
    pub level: Option<LogLevelArg>,
    /// The last N lines (at most 2000; the output is also cut to 64 KiB).
    #[arg(long, value_name = "N", default_value_t = 200, value_parser = clap::value_parser!(u32).range(1..=2000))]
    pub lines: u32,
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogLevelArg {
    Error,
    Info,
    Debug,
}

#[derive(Args, Clone, Debug, PartialEq)]
pub struct CrashesArgs {
    /// How far back: 30s, 10m, 1h (at most 1h).
    #[arg(long, value_name = "TIME", default_value = "10m", value_parser = parse_since)]
    pub since: u32,
    /// One app's crashes (bundle id; Android: package name). Required on a
    /// real Android phone.
    #[arg(long, value_name = "ID")]
    pub app: Option<String>,
}

/// `30s`, `5m`, `1h` (or bare seconds) as seconds, at most an hour.
pub fn parse_since(text: &str) -> Result<u32, String> {
    let text = text.trim();
    let (number, unit) = match text.char_indices().last() {
        Some((at, 's')) => (&text[..at], 1),
        Some((at, 'm')) => (&text[..at], 60),
        Some((at, 'h')) => (&text[..at], 3600),
        _ => (text, 1),
    };
    let secs = number.parse::<u32>().ok().and_then(|n| n.checked_mul(unit)).filter(|s| *s > 0);
    match secs {
        Some(secs) if secs <= MAX_SINCE_SECS => Ok(secs),
        Some(_) => Err("at most 1h".into()),
        None => Err(format!("`{text}` is not a time like 30s, 5m or 1h")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_reads_as_seconds_up_to_an_hour() {
        assert_eq!(parse_since("30s"), Ok(30));
        assert_eq!(parse_since("5m"), Ok(300));
        assert_eq!(parse_since("1h"), Ok(3600));
        assert_eq!(parse_since("90"), Ok(90));
        assert!(parse_since("2h").is_err());
        assert!(parse_since("0s").is_err());
        assert!(parse_since("soon").is_err());
    }
}
