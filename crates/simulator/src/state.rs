//! Device-state changes, as data: what `sim location`, `appearance`,
//! `privacy`, `push` and `status-bar` ask of a device, checked before any
//! command line is built. [`simctl_state`](crate::simctl_state) and
//! [`android::state`](crate::android::state) turn a change into argv; erase is
//! the hub's (it shuts the device down and boots it again).
//!
//! Every name that reaches a command line comes from a table here or passes a
//! strict charset check: `adb shell` joins its words for the device's `sh`,
//! so argv alone is not safe there.

/// One change to a device's state.
#[derive(Clone, Debug, PartialEq)]
pub enum StateChange {
    /// Degrees.
    Location { lat: f64, lon: f64 },
    ClearLocation,
    Appearance { dark: bool },
    Privacy { action: PrivacyAction, service: Service, app: String },
    /// `payload`: a checked JSON object with `aps` (see [`check_push`]).
    Push { app: String, payload: String },
    StatusBar { clean: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivacyAction {
    Grant,
    Revoke,
    /// Back to "not asked yet", for that app only.
    Reset,
}

impl PrivacyAction {
    pub fn word(self) -> &'static str {
        match self {
            Self::Grant => "grant",
            Self::Revoke => "revoke",
            Self::Reset => "reset",
        }
    }
}

/// Why a change is not made: the request is unusable, or the device cannot
/// do it (one wording per platform gap).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reject {
    BadInput(String),
    Unsupported(String),
}

/// A privacy service, by the name `simctl privacy` uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    Calendar,
    Contacts,
    Location,
    LocationAlways,
    Photos,
    PhotosAdd,
    MediaLibrary,
    Microphone,
    Motion,
    Reminders,
    Siri,
    Camera,
    Notifications,
}

/// Every service, with its name.
const SERVICES: [(&str, Service); 13] = [
    ("calendar", Service::Calendar),
    ("contacts", Service::Contacts),
    ("location", Service::Location),
    ("location-always", Service::LocationAlways),
    ("photos", Service::Photos),
    ("photos-add", Service::PhotosAdd),
    ("media-library", Service::MediaLibrary),
    ("microphone", Service::Microphone),
    ("motion", Service::Motion),
    ("reminders", Service::Reminders),
    ("siri", Service::Siri),
    ("camera", Service::Camera),
    ("notifications", Service::Notifications),
];

impl Service {
    pub fn parse(name: &str) -> Result<Self, Reject> {
        let name = name.trim().to_ascii_lowercase();
        SERVICES.iter().find(|(n, _)| *n == name).map(|(_, s)| *s).ok_or_else(|| {
            let known: Vec<&str> = SERVICES.iter().map(|(n, _)| *n).collect();
            Reject::BadInput(format!("`{name}` is not a privacy service (one of: {})", known.join(", ")))
        })
    }

    pub fn name(self) -> &'static str {
        SERVICES.iter().find(|(_, s)| *s == self).map_or("", |(n, _)| n)
    }

    /// The Android permissions a service stands for (empty: none).
    pub fn android_permissions(self) -> &'static [&'static str] {
        match self {
            Self::Calendar => &["android.permission.READ_CALENDAR", "android.permission.WRITE_CALENDAR"],
            Self::Contacts => &["android.permission.READ_CONTACTS", "android.permission.WRITE_CONTACTS"],
            Self::Location => &["android.permission.ACCESS_FINE_LOCATION", "android.permission.ACCESS_COARSE_LOCATION"],
            Self::LocationAlways => &[
                "android.permission.ACCESS_FINE_LOCATION",
                "android.permission.ACCESS_COARSE_LOCATION",
                "android.permission.ACCESS_BACKGROUND_LOCATION",
            ],
            Self::Photos | Self::MediaLibrary => &["android.permission.READ_MEDIA_IMAGES", "android.permission.READ_MEDIA_VIDEO"],
            Self::Microphone => &["android.permission.RECORD_AUDIO"],
            Self::Motion => &["android.permission.ACTIVITY_RECOGNITION"],
            Self::Camera => &["android.permission.CAMERA"],
            Self::Notifications => &["android.permission.POST_NOTIFICATIONS"],
            Self::PhotosAdd | Self::Reminders | Self::Siri => &[],
        }
    }
}

/// A latitude and longitude on Earth, in degrees.
pub fn check_location(lat: f64, lon: f64) -> Result<(), Reject> {
    if lat.is_finite() && lon.is_finite() && (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
        Ok(())
    } else {
        Err(Reject::BadInput(format!("{lat}, {lon} is not a place: latitude is -90..90, longitude -180..180")))
    }
}

/// An app id: a bundle identifier (`com.example.App-1`) or an Android
/// package (`com.example.app`): letters, digits, `.`, `-`, `_`, starting with
/// a letter or digit — nothing an option, a path or a shell word is made of.
pub fn check_app(app: &str) -> Result<(), Reject> {
    let ok = app.len() <= 255
        && app.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && app.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if ok { Ok(()) } else { Err(Reject::BadInput(format!("`{app}` is not an app id (a bundle identifier or package name)"))) }
}

/// `com.example.app`: letters, digits, `_`, dot-separated, each part starting
/// with a letter.
pub fn is_package(name: &str) -> bool {
    name.split('.').count() >= 2
        && name.split('.').all(|part| {
            part.chars().next().is_some_and(|c| c.is_ascii_alphabetic()) && part.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

/// The most a push payload may be (Apple's limit for a notification).
pub const MAX_PUSH: usize = 4096;

/// A push payload: a JSON object with an `aps` object, at most [`MAX_PUSH`]
/// bytes.
pub fn check_push(payload: &str) -> Result<(), Reject> {
    if payload.len() > MAX_PUSH {
        return Err(Reject::BadInput(format!("a push payload is at most {MAX_PUSH} bytes (this one is {})", payload.len())));
    }
    let value: serde_json::Value =
        serde_json::from_str(payload).map_err(|e| Reject::BadInput(format!("the push payload is not JSON: {e}")))?;
    if value.get("aps").is_some_and(serde_json::Value::is_object) {
        Ok(())
    } else {
        Err(Reject::BadInput("a push payload is a JSON object with an `aps` object in it".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn services_parse_by_their_simctl_names() {
        assert_eq!(Service::parse(" Location-Always "), Ok(Service::LocationAlways));
        assert_eq!(Service::LocationAlways.name(), "location-always");
        assert!(matches!(Service::parse("keychain"), Err(Reject::BadInput(e)) if e.contains("photos")));
        assert!(Service::Siri.android_permissions().is_empty());
        assert_eq!(Service::Camera.android_permissions(), ["android.permission.CAMERA"]);
    }

    #[test]
    fn places_are_on_earth() {
        assert!(check_location(10.77, 106.70).is_ok());
        for (lat, lon) in [(91.0, 0.0), (0.0, -181.0), (f64::NAN, 0.0), (0.0, f64::INFINITY)] {
            assert!(check_location(lat, lon).is_err(), "{lat} {lon}");
        }
    }

    #[test]
    fn app_ids_carry_no_shell_or_option() {
        for good in ["com.example.App-1", "org.chromium.chrome", "a.b_c"] {
            assert!(check_app(good).is_ok(), "{good}");
        }
        for bad in ["", "-x", "a;rm", "a b", "a$(id)", "a`id`", "../a", "a'b"] {
            assert!(check_app(bad).is_err(), "{bad}");
        }
        assert!(is_package("com.example.app") && !is_package("com") && !is_package("com.-app"));
    }

    #[test]
    fn a_push_is_an_object_with_aps() {
        assert!(check_push(r#"{"aps":{"alert":"Hi"}}"#).is_ok());
        assert!(check_push(r#"{"alert":"Hi"}"#).is_err());
        assert!(check_push(r#"{"aps":"Hi"}"#).is_err());
        assert!(check_push("not json").is_err());
        let big = format!(r#"{{"aps":{{"alert":"{}"}}}}"#, "x".repeat(MAX_PUSH));
        assert!(matches!(check_push(&big), Err(Reject::BadInput(e)) if e.contains("4096")));
    }
}
