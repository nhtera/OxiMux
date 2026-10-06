//! What the scrcpy server says on stderr (it logs at `warn`), sorted into
//! what the panel can explain. Read on a thread of its own for the server's
//! whole life, so the pipe never fills and stalls it.

/// Shown once per session when the phone refuses injected input.
pub const INPUT_BLOCKED: &str = "This phone blocks input from USB debugging. On Xiaomi/HyperOS turn on \
     “USB debugging (Security settings)” in Developer options, then reconnect (some phones need a reboot). \
     The picture keeps working.";

/// A line of the server's log, classified.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServerLog {
    /// Input injection was refused (`INJECT_EVENTS`): an OEM setting, not a bug.
    InputBlocked,
    Other,
}

pub fn classify(line: &str) -> ServerLog {
    if line.contains("INJECT_EVENTS") {
        ServerLog::InputBlocked
    } else {
        ServerLog::Other
    }
}

/// Read the server's stderr to its end, calling `on_blocked` the first time
/// input is refused; every line also goes to the debug log.
pub fn drain(stderr: impl std::io::Read, mut on_blocked: impl FnMut()) {
    use std::io::BufRead as _;
    let (mut reader, mut buf, mut told) = (std::io::BufReader::new(stderr), Vec::new(), false);
    // To the end, whatever the bytes: stopping early would close the pipe,
    // and the server's next write would kill it (and the stream).
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        }
        let line = String::from_utf8_lossy(&buf);
        let line = line.trim_end();
        tracing::debug!(target: "oximux_simulator::android::server", "{line}");
        if !told && classify(line) == ServerLog::InputBlocked {
            told = true;
            on_blocked();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines as the scrcpy server prints them on an OEM build that refuses
    /// injection, and ordinary warnings.
    #[test]
    fn a_refused_injection_is_told_once() {
        let log = "WARN: Display: unknown\n\
java.lang.SecurityException: Injecting input events requires the caller (or the source of the instrumentation, if any) to have the INJECT_EVENTS permission.\n\
\tat android.os.Parcel.createExceptionOrNull(Parcel.java:3069)\n\
ERROR: Could not inject event (INJECT_EVENTS)\n";
        let mut told = 0;
        drain(log.as_bytes(), || told += 1);
        assert_eq!(told, 1);
        assert_eq!(classify("WARN: Display: unknown"), ServerLog::Other);
    }

    /// A line that is not UTF-8 does not end the drain (the pipe must stay
    /// read to the end): what follows it is still seen.
    #[test]
    fn invalid_bytes_do_not_stop_the_drain() {
        let mut log = b"WARN: \xff\xfe broken\n".to_vec();
        log.extend_from_slice(b"ERROR: INJECT_EVENTS denied\n");
        let mut told = 0;
        drain(log.as_slice(), || told += 1);
        assert_eq!(told, 1);
    }
}
