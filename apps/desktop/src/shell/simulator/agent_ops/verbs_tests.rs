use super::*;

/// The verdict on a phone's input: refused when it blocks input (before
/// or during the wait); otherwise trusted once, and not waited on again.
#[test]
fn a_phone_that_blocks_input_is_refused_and_one_that_takes_it_is_trusted() {
    use std::cell::Cell;
    let run = |blocked_before: bool, blocked_during: bool, proven: bool| {
        let (blocked, waited, marked) = (Cell::new(blocked_before), Cell::new(false), Cell::new(false));
        let wait = async {
            waited.set(true);
            if blocked_during {
                blocked.set(true);
            }
        };
        let out = futures::executor::block_on(input_verdict(|| blocked.get(), || proven, || marked.set(true), wait));
        (out.is_ok(), waited.get(), marked.get())
    };
    assert_eq!(run(true, false, false), (false, false, false), "known blocked: refused, no wait");
    assert_eq!(run(false, true, false), (false, true, false), "refused during the wait");
    assert_eq!(run(false, false, false), (true, true, true), "taken: trusted from now on");
    assert_eq!(run(false, false, true), (true, false, true), "already trusted: no wait");
}

/// An iPhone's app verbs never reach `simctl` (they have `devicectl`
/// arms, whose install asks the user first, as Android's does).
#[test]
fn an_iphones_app_verbs_never_reach_simctl() {
    let iphone = DeviceId("iosdev:00008110-001A2C3E0A88401E".into());
    assert!(matches!(simctl_udid(&iphone), Err(SimErrorWire::Unavailable(_))));
    assert!(simctl_udid(&DeviceId("81CE1BE8-E38A-4BA8-8AAB-5DACA07576B3".into())).is_ok());
}

#[test]
fn a_bundle_id_is_never_an_option_or_a_path() {
    assert!(check_bundle_id("com.example.App-1_x").is_ok());
    for bad in ["", "-x", "../a", "a b", "a/b"] {
        assert!(check_bundle_id(bad).is_err(), "{bad}");
    }
}
