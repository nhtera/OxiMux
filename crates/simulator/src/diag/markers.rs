//! Lines that mean an app crashed, or came close: matched by substring, one
//! const table (no patterns to load, no regex).

/// What a marked line means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The app died: a crash.
    Fatal,
    /// The app is in trouble but alive (hung, an unhandled rejection).
    Warning,
}

/// Each marker and what it means, checked in order.
pub const MARKERS: &[(&str, Kind)] = &[
    // iOS: an Objective-C exception nobody caught, a Swift trap.
    ("Terminating app due to uncaught exception", Kind::Fatal),
    ("Fatal error:", Kind::Fatal),
    // Android: a Java/Kotlin exception, a native signal.
    ("FATAL EXCEPTION", Kind::Fatal),
    ("Fatal signal", Kind::Fatal),
    // React Native, either platform.
    ("Unhandled JS Exception", Kind::Fatal),
    ("ANR in ", Kind::Warning),
    ("Possible Unhandled Promise Rejection", Kind::Warning),
    // Flutter.
    ("EXCEPTION CAUGHT BY", Kind::Warning),
];

/// What `line` marks, if anything.
pub fn classify(line: &str) -> Option<Kind> {
    MARKERS.iter().find(|(marker, _)| line.contains(marker)).map(|(_, kind)| *kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_marker_is_found_in_a_real_line() {
        let lines = [
            ("*** Terminating app due to uncaught exception 'NSInvalidArgumentException', reason: 'x'", Kind::Fatal),
            ("Crashy[61726:59a99a] (libswiftCore.dylib) Swift/ContiguousArrayBuffer.swift:691: Fatal error: Index out of range", Kind::Fatal),
            ("10-12 15:54:29.797  4321  4321 E AndroidRuntime: FATAL EXCEPTION: main", Kind::Fatal),
            ("10-12 15:54:29.797  4321  4321 F libc    : Fatal signal 11 (SIGSEGV), code 1 (SEGV_MAPERR)", Kind::Fatal),
            ("E ReactNativeJS: Unhandled JS Exception: TypeError: undefined is not a function", Kind::Fatal),
            ("E ActivityManager: ANR in com.example.app (com.example.app/.MainActivity)", Kind::Warning),
            ("W ReactNativeJS: Possible Unhandled Promise Rejection (id: 0):", Kind::Warning),
            ("I flutter : ══╡ EXCEPTION CAUGHT BY WIDGETS LIBRARY ╞════", Kind::Warning),
        ];
        for (line, kind) in lines {
            assert_eq!(classify(line), Some(kind), "{line}");
        }
    }

    #[test]
    fn ordinary_lines_are_not_marked() {
        for line in ["I ActivityManager: Start proc 4321:com.example.app", "fatal is a word", "an error: something failed"] {
            assert_eq!(classify(line), None, "{line}");
        }
    }
}
