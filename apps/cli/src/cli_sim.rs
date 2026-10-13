//! `oximux sim` arguments for the verify loop (v27): `--settle`, `@eN` refs,
//! `wait` and `assert`. Kept beside `cli.rs` so that file stays small.

use clap::{ArgGroup, Args};
use oximux_remote_proto::simulator::{SimCheckWire, SimTargetWire};

use crate::cli::{ExactArgs, FlowCommand, SimCommand};

/// `--settle` on an action.
#[derive(Args, Debug, Clone, Default, PartialEq, Eq)]
pub struct SettleArgs {
    /// Wait for the screen to go idle, then print what changed: `+` appeared,
    /// `-` went, with `@eN` refs to act on next.
    #[arg(long)]
    pub settle: bool,
    /// The longest to wait for it (default 5000 on a simulator, 6000 on
    /// Android or an iPhone, 8000 for launch and open-url; at most 60000).
    #[arg(long, value_name = "MS", requires = "settle")]
    pub settle_timeout: Option<u32>,
}

/// An `@eN` ref, as `ax -i` and a settle diff print it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RefArg(pub u32);

impl std::str::FromStr for RefArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        parse_ref(s).map(RefArg).ok_or_else(|| format!("`{s}` is not an @eN ref (see `oximux sim ax -i`)"))
    }
}

/// `@e7` → 7.
pub fn parse_ref(s: &str) -> Option<u32> {
    s.strip_prefix("@e").filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())).and_then(|n| n.parse().ok())
}

/// `tap`'s first word: an x coordinate, or a ref.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TapArg {
    X(f64),
    Ref(u32),
}

impl std::str::FromStr for TapArg {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if let Some(r) = parse_ref(s) {
            return Ok(Self::Ref(r));
        }
        s.parse().map(Self::X).map_err(|_| format!("`{s}` is neither a coordinate (points) nor an @eN ref"))
    }
}

/// The element `wait` and `assert` name: a ref, a label or an identifier.
#[derive(Args, Debug, Clone, PartialEq)]
#[command(group(ArgGroup::new("element").required(true).multiple(true).args(["r", "label", "id", "role"])))]
pub struct ElementArgs {
    /// An `@eN` ref.
    #[arg(value_name = "@eN", conflicts_with_all = ["label", "id", "role", "exact", "nth"])]
    pub r: Option<RefArg>,
    /// An accessibility label: exact match first, then a case-insensitive
    /// substring (only exact with --exact).
    #[arg(long, value_name = "TEXT", conflicts_with = "id")]
    pub label: Option<String>,
    /// An accessibility identifier, exact.
    #[arg(long, value_name = "AXID")]
    pub id: Option<String>,
    #[command(flatten)]
    pub exact: ExactArgs,
}

impl ElementArgs {
    pub fn target(&self) -> SimTargetWire {
        if let Some(exact) = self.exact.target(self.label.as_ref(), self.id.as_ref()) {
            return exact;
        }
        match (&self.r, &self.label, &self.id) {
            (Some(r), _, _) => SimTargetWire::Ref(r.0),
            (_, Some(label), _) => SimTargetWire::Label(label.clone()),
            (_, _, Some(id)) => SimTargetWire::Id(id.clone()),
            // clap's group makes one of them required.
            (None, None, None) => SimTargetWire::Label(String::new()),
        }
    }
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct WaitArgs {
    #[command(flatten)]
    pub element: ElementArgs,
    /// Wait for it to go away instead.
    #[arg(long)]
    pub gone: bool,
    /// Give up after this long (at most 60000).
    #[arg(long, value_name = "MS", default_value_t = 10_000)]
    pub timeout_ms: u32,
}

#[derive(Args, Debug, Clone, PartialEq)]
pub struct AssertArgs {
    #[command(flatten)]
    pub element: ElementArgs,
    /// It is not on screen.
    #[arg(long, conflicts_with_all = ["text", "enabled"])]
    pub gone: bool,
    /// Its text (a field's value, else its label) is exactly this.
    #[arg(long, value_name = "T", conflicts_with = "enabled")]
    pub text: Option<String>,
    /// It is on screen and enabled.
    #[arg(long)]
    pub enabled: bool,
}

impl AssertArgs {
    pub fn check(&self) -> SimCheckWire {
        match (&self.text, self.gone, self.enabled) {
            (Some(text), _, _) => SimCheckWire::Text(text.clone()),
            (None, true, _) => SimCheckWire::Gone,
            (None, false, true) => SimCheckWire::Enabled,
            (None, false, false) => SimCheckWire::Exists,
        }
    }
}

impl SimCommand {
    /// The verify-loop feature this command uses, named for the "host too
    /// old" refusal — `None` when an older host serves it as is.
    pub fn verify_feature(&self) -> Option<&'static str> {
        match self {
            Self::Wait(_) => Some("sim wait"),
            Self::Assert(_) => Some("sim assert"),
            Self::Ax { interactive: true, .. } => Some("sim ax -i"),
            Self::Tap { x: Some(TapArg::Ref(_)), .. } => Some("sim tap @eN"),
            // Only a real ref: `type hello world` (unquoted) is a usage error
            // whatever the host's version.
            Self::Type { text, settle, .. } => {
                (text.len() > 1 && parse_ref(&text[0]).is_some()).then_some("sim type @eN").or_else(|| settle_of(settle))
            }
            Self::Tap { settle: s, .. }
            | Self::Swipe { settle: s, .. }
            | Self::Button { settle: s, .. }
            | Self::Rotate { settle: s, .. }
            | Self::Launch { settle: s, .. }
            | Self::OpenUrl { settle: s, .. } => settle_of(s),
            Self::Status
            | Self::Devices { .. }
            | Self::Attach { .. }
            | Self::Detach
            | Self::WaitConsent { .. }
            | Self::Screenshot { .. }
            | Self::Ax { .. }
            | Self::Install { .. }
            | Self::Shutdown { .. } => None,
            Self::Location(_) | Self::Appearance { .. } | Self::Privacy(_) | Self::Push(_) | Self::StatusBar { .. } | Self::Erase { .. } => None,
            Self::Logs(_) | Self::Crashes(_) => None,
            // v30, and `run`'s steps are gated one by one.
            Self::Run(_) | Self::Flow(_) => None,
            Self::Exec(_) => None,
        }
    }

    /// The v30 feature this command uses (a recording's verbs, an exact
    /// selector), named for the "host too old" refusal. `flow export` and
    /// `run` themselves need none: export is local, and a run's steps are
    /// gated one by one.
    pub fn flow_feature(&self) -> Option<&'static str> {
        match self {
            Self::Flow(FlowCommand::Start | FlowCommand::Stop { .. } | FlowCommand::Status) => Some("sim flow"),
            Self::Tap { exact, .. } if exact.used() => Some("sim … --exact/--role/--nth"),
            Self::Wait(WaitArgs { element, .. }) | Self::Assert(AssertArgs { element, .. }) if element.exact.used() => {
                Some("sim … --exact/--role/--nth")
            }
            _ => None,
        }
    }

    /// `exec` (v31).
    pub fn is_exec(&self) -> bool {
        matches!(self, Self::Exec(_))
    }

    /// The v29 diagnostics verbs (`logs`, `crashes`).
    pub fn is_diag(&self) -> bool {
        matches!(self, Self::Logs(_) | Self::Crashes(_))
    }

    /// The v28 device-state verbs (`location`, `appearance`, …).
    pub fn is_device_state(&self) -> bool {
        matches!(self, Self::Location(_) | Self::Appearance { .. } | Self::Privacy(_) | Self::Push(_) | Self::StatusBar { .. } | Self::Erase { .. })
    }
}

impl SimCommand {
    /// An action's `--settle` arguments (`None` for anything that does not
    /// settle).
    pub fn settle_args(&self) -> Option<&SettleArgs> {
        match self {
            Self::Tap { settle, .. }
            | Self::Swipe { settle, .. }
            | Self::Type { settle, .. }
            | Self::Button { settle, .. }
            | Self::Rotate { settle, .. }
            | Self::Launch { settle, .. }
            | Self::OpenUrl { settle, .. } => Some(settle),
            Self::Status
            | Self::Devices { .. }
            | Self::Attach { .. }
            | Self::Detach
            | Self::WaitConsent { .. }
            | Self::Screenshot { .. }
            | Self::Ax { .. }
            | Self::Install { .. }
            | Self::Shutdown { .. }
            | Self::Wait(_)
            | Self::Assert(_)
            | Self::Location(_)
            | Self::Appearance { .. }
            | Self::Privacy(_)
            | Self::Push(_)
            | Self::StatusBar { .. }
            | Self::Erase { .. }
            | Self::Logs(_)
            | Self::Crashes(_)
            | Self::Run(_)
            | Self::Flow(_)
            | Self::Exec(_) => None,
        }
    }

    /// [`Self::settle_args`], to change (`run --settle` turns it on).
    pub fn settle_args_mut(&mut self) -> Option<&mut SettleArgs> {
        match self {
            Self::Tap { settle, .. }
            | Self::Swipe { settle, .. }
            | Self::Type { settle, .. }
            | Self::Button { settle, .. }
            | Self::Rotate { settle, .. }
            | Self::Launch { settle, .. }
            | Self::OpenUrl { settle, .. } => Some(settle),
            _ => None,
        }
    }
}

fn settle_of(s: &SettleArgs) -> Option<&'static str> {
    s.settle.then_some("sim … --settle")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser as _;

    fn sim(args: &[&str]) -> Result<SimCommand, clap::Error> {
        let argv = ["oximux", "sim"].into_iter().chain(args.iter().copied());
        Cli::try_parse_from(argv).map(|cli| match cli.command {
            crate::cli::Command::Sim { command, .. } => command,
            other => panic!("not sim: {other:?}"),
        })
    }

    #[test]
    fn a_ref_reads_as_at_e_and_a_number() {
        assert_eq!(parse_ref("@e7"), Some(7));
        for bad in ["e7", "@e", "@ex", "@e-1", "@E7", "7"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
        assert_eq!("@e12".parse::<TapArg>(), Ok(TapArg::Ref(12)));
        assert_eq!("201.5".parse::<TapArg>(), Ok(TapArg::X(201.5)));
        assert!("Sign".parse::<TapArg>().is_err());
    }

    #[test]
    fn only_the_new_features_need_a_new_host() {
        for (args, feature) in [
            (&["tap", "@e7"][..], Some("sim tap @eN")),
            (&["tap", "10", "20"], None),
            (&["tap", "--label", "OK", "--settle"], Some("sim … --settle")),
            (&["swipe", "1", "2", "3", "4", "--settle", "--settle-timeout", "900"], Some("sim … --settle")),
            (&["type", "hello"], None),
            (&["type", "@e3", "hello"], Some("sim type @eN")),
            (&["type", "hello", "--settle"], Some("sim … --settle")),
            (&["type", "hello", "world"], None),
            (&["ax", "-i"], Some("sim ax -i")),
            (&["ax", "--flat"], None),
            (&["wait", "--label", "Home"], Some("sim wait")),
            (&["assert", "@e2", "--enabled"], Some("sim assert")),
            (&["launch", "com.a.b", "--settle"], Some("sim … --settle")),
            (&["button", "home"], None),
        ] {
            assert_eq!(sim(args).unwrap().verify_feature(), feature, "{args:?}");
        }
    }

    #[test]
    fn wait_and_assert_name_exactly_one_element() {
        assert!(sim(&["wait"]).is_err(), "no element");
        assert!(sim(&["wait", "@e1", "--label", "x"]).is_err(), "two elements");
        assert!(sim(&["assert", "--id", "a", "--gone", "--text", "x"]).is_err());
        assert!(sim(&["tap", "1", "2", "--settle-timeout", "5"]).is_err(), "a timeout without --settle");
        let SimCommand::Wait(w) = sim(&["wait", "--id", "home", "--gone", "--timeout-ms", "3000"]).unwrap() else { panic!() };
        assert_eq!((w.element.target(), w.gone, w.timeout_ms), (SimTargetWire::Id("home".into()), true, 3000));
        let SimCommand::Assert(a) = sim(&["assert", "@e4", "--text", "Hi"]).unwrap() else { panic!() };
        assert_eq!((a.element.target(), a.check()), (SimTargetWire::Ref(4), SimCheckWire::Text("Hi".into())));
        let SimCommand::Assert(a) = sim(&["assert", "--label", "Next"]).unwrap() else { panic!() };
        assert_eq!(a.check(), SimCheckWire::Exists);
    }
}
