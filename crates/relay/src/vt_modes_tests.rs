use super::*;

fn tracked(bytes: &[u8]) -> ModeTracker {
    let mut t = ModeTracker::new();
    t.feed(bytes);
    t
}

fn modes(bytes: &[u8]) -> Modes {
    tracked(bytes).modes().clone()
}

#[test]
fn every_tracked_mode_turns_on_and_off() {
    for (mode, default) in TRACKED_MODES {
        let on = modes(format!("\x1b[?{mode}h").as_bytes());
        assert_eq!(on.dec(*mode), Some(true), "{mode}");
        let off = modes(format!("\x1b[?{mode}h\x1b[?{mode}l").as_bytes());
        assert_eq!(off.dec(*mode), Some(false), "{mode}");
        assert_eq!(Modes::default().dec(*mode), Some(*default));
    }
}

#[test]
fn several_modes_in_one_sequence_and_the_alt_screens() {
    let m = modes(b"\x1b[?1049;2004;1006h");
    assert_eq!(m.alt_screen, Some(1049));
    assert_eq!(m.dec(2004), Some(true));
    assert_eq!(m.mouse_encoding, Some(1006));
    // The older alt-screen spellings are ignored, as the client emulator does.
    assert_eq!(modes(b"\x1b[?47h").alt_screen, None);
    assert_eq!(modes(b"\x1b[?1047h").alt_screen, None);
}

#[test]
fn mouse_modes_are_one_at_a_time() {
    let m = modes(b"\x1b[?1000h\x1b[?1003h\x1b[?1005h\x1b[?1006h");
    assert_eq!((m.mouse_tracking, m.mouse_encoding), (Some(1003), Some(1006)));
    // Turning off one that is not in force changes nothing.
    assert_eq!(modes(b"\x1b[?1003h\x1b[?1000l").mouse_tracking, Some(1003));
    assert_eq!(modes(b"\x1b[?1003h\x1b[?1003l").mouse_tracking, None);
}

#[test]
fn escapes_with_intermediates_and_string_ends_follow_the_emulator() {
    // `ESC ( =` designates a charset; it is not DECKPAM.
    assert!(!modes(b"\x1b(=").keypad_application);
    assert_eq!(modes(b"\x1b(B\x1b[?2004h").dec(2004), Some(true));
    // An ESC inside a string ends it, and what follows is a sequence again.
    assert_eq!(modes(b"\x1b]0;title\x1b[?2004h").dec(2004), Some(true));
    // A mode number past u16 is no mode; it must not wrap onto ?1049.
    assert_eq!(modes(b"\x1b[?66585h").alt_screen, None);
}

#[test]
fn scroll_region_keypad_and_keyboard_stack() {
    assert_eq!(modes(b"\x1b[5;20r").scroll_region, Some((5, 20)));
    assert_eq!(modes(b"\x1b[5;20r\x1b[r").scroll_region, None);
    assert_eq!(modes(b"\x1b[;20r").scroll_region, Some((1, 20)));
    assert_eq!(modes(b"\x1b[20;5r").scroll_region, None, "inverted bounds are ignored");
    assert!(modes(b"\x1b=").keypad_application);
    assert!(!modes(b"\x1b=\x1b>").keypad_application);
    assert_eq!(modes(b"\x1b[>1u\x1b[>3u").keyboard_flags(), 3);
    assert_eq!(modes(b"\x1b[>1u\x1b[>3u\x1b[<u").keyboard_flags(), 1);
    assert_eq!(modes(b"\x1b[>1u\x1b[<5u").keyboard_flags(), 0);
    assert_eq!(modes(b"\x1b[>1u\x1b[=4;2u").keyboard_flags(), 5);
}

#[test]
fn ris_resets_and_decstr_softly_resets() {
    let t = tracked(b"\x1b[?1049h\x1b[?2004h\x1b[3;9r\x1bc");
    assert_eq!(t.modes(), &Modes::default());
    assert_eq!(t.alt_entry_end(), None);
    let soft = modes(b"\x1b[?25l\x1b[3;9r\x1b=\x1b[?2004h\x1b[!p");
    assert_eq!(soft.dec(25), Some(true));
    assert_eq!(soft.scroll_region, None);
    assert!(!soft.keypad_application);
    assert_eq!(soft.dec(2004), Some(true), "DECSTR leaves bracketed paste alone");
}

#[test]
fn sequences_inside_strings_and_other_csis_change_nothing() {
    // Mode-looking text inside a string is just text (an ESC would end it).
    assert_eq!(modes(b"\x1b]0;[?1049h title\x07"), Modes::default());
    assert_eq!(modes(b"\x1bP[?2004h data\x1b\\"), Modes::default());
    assert_eq!(modes(b"\x1b[38;5;196m\x1b[2J\x1b[H\x1b[1049h"), Modes::default());
    assert_eq!(modes(b"\x1b[?1049\x18h"), Modes::default(), "CAN aborts");
}

#[test]
fn a_split_at_every_byte_tracks_the_same() {
    let whole = b"\x1b[?1049h\x1b]2;x\x1b\\\x1b[2;30r\x1b[?2004;1006h\x1b=\x1b[>7u\x1b[?25l";
    let expect = modes(whole);
    for split in 0..=whole.len() {
        let mut t = ModeTracker::new();
        t.feed(&whole[..split]);
        t.feed(&whole[split..]);
        assert_eq!(t.modes(), &expect, "split at {split}");
    }
}

#[test]
fn the_alt_entry_offset_is_where_the_entry_ends() {
    let mut t = ModeTracker::starting_at(1000);
    t.feed(b"abc\x1b[?1049h");
    assert_eq!(t.alt_entry_end(), Some(1000 + 11));
    // Re-entering while already in it does not move the entry.
    t.feed(b"\x1b[?1049h");
    assert_eq!(t.alt_entry_end(), Some(1011));
    t.feed(b"\x1b[?1049l");
    assert_eq!(t.alt_entry_end(), None);
}

#[test]
fn random_bytes_never_panic_and_stay_bounded() {
    let mut seed = 0x9E37_79B9_7F4A_7C15u64;
    let mut t = ModeTracker::new();
    for _ in 0..200_000 {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        // Bias toward escape bytes so sequences actually form.
        let b = match seed % 8 {
            0 => 0x1b,
            1 => b'[',
            2 => b'?',
            3 => b'>',
            _ => (seed >> 8) as u8,
        };
        t.feed(&[b]);
    }
    assert!(t.params.len() <= MAX_PARAMS);
    assert!(t.modes().keyboard.len() <= MAX_KEYBOARD_STACK);
}

#[test]
fn a_replay_starts_on_a_clean_line() {
    // Cut mid-escape and mid-line: start after the first newline outside it.
    let snap = b"5;1Hpartial\x1b[0m line\nnext line\n";
    assert_eq!(&snap[align_start(snap)..], b"next line\n");
    // A newline inside an OSC does not count.
    let osc = b"x\x1b]0;a\nb\x07tail\nok";
    assert_eq!(&osc[align_start(osc)..], b"ok");
    // No newline at all: the first UTF-8 boundary.
    let cut = "éé".as_bytes();
    assert_eq!(align_start(&cut[1..]), 1);
    assert_eq!(align_start(b""), 0);
}

#[test]
fn the_postamble_emits_only_what_the_body_lacks() {
    let live = modes(b"\x1b[?1049h\x1b[3;20r\x1b[?2004h\x1b[?1006h\x1b=\x1b[?25l");
    assert!(postamble(&live, &live).is_empty());
    assert!(postamble(&Modes::default(), &Modes::default()).is_empty());

    let post = String::from_utf8(postamble(&Modes::default(), &live)).unwrap();
    assert!(post.starts_with("\x1b[?1049h\x1b7\x1b[3;20r\x1b8"), "{post:?}");
    assert!(post.contains("\x1b[?2004h") && post.contains("\x1b[?1006h") && post.contains("\x1b="));
    assert!(post.ends_with("\x1b[?25l"), "{post:?}");

    // Modes that move nothing are not bracketed: the bracket would overwrite a
    // cursor the program saved itself.
    let post = String::from_utf8(postamble(&Modes::default(), &modes(b"\x1b[?2004h"))).unwrap();
    assert_eq!(post, "\x1b[?2004h");

    // The body already in the alternate screen with paste on: not repeated.
    let body = modes(b"\x1b[?1049h\x1b[?2004h");
    let post = String::from_utf8(postamble(&body, &live)).unwrap();
    assert!(!post.contains("1049") && !post.contains("2004"), "{post:?}");
}

#[test]
fn synchronized_output_is_never_restored() {
    let live = modes(b"\x1b[?2026h");
    assert!(!String::from_utf8(postamble(&Modes::default(), &live)).unwrap().contains("2026"));
}

#[test]
fn the_postamble_turns_off_what_the_body_left_on() {
    let body = modes(b"\x1b[?2004h\x1b[?1049h");
    let live = Modes::default();
    let post = String::from_utf8(postamble(&body, &live)).unwrap();
    assert!(post.contains("\x1b[?1049l") && post.contains("\x1b[?2004l"), "{post:?}");
}

#[test]
fn the_sequence_in_flight_is_located_by_its_esc() {
    let mut t = ModeTracker::new();
    t.feed(b"abc");
    assert_eq!(t.seq_start(), None);
    t.feed(b"\x1b[3");
    assert_eq!(t.seq_start(), Some(3));
    t.feed(b"1mxy");
    assert_eq!(t.seq_start(), None, "finished");
    // An OSC's start stays put across an ESC inside the string...
    t.feed(b"\x1b]0;title");
    assert_eq!(t.seq_start(), Some(10));
    t.feed(b"\x1b");
    assert_eq!(t.seq_start(), Some(10));
    t.feed(b"\\");
    assert_eq!(t.seq_start(), None);
    // ...and a new ESC inside a CSI starts a new sequence.
    t.feed(b"\x1b[1\x1b[");
    assert_eq!(t.seq_start(), Some(t.offset() - 2));
}
