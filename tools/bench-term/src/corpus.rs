//! Generated output corpora — nothing committed, any size on demand.

/// Plain ASCII lines, shell-log-like.
pub fn plain(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        v.extend_from_slice(format!("[{i:06}] building target/debug/deps/libexample-{i:x}.rlib ok\r\n").as_bytes());
    }
    v
}

/// SGR-heavy lines: a colour change every word, truecolor and 256-colour.
pub fn styled(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        for w in 0..8 {
            let c = (i * 8 + w) % 256;
            v.extend_from_slice(format!("\x1b[38;5;{c}mword{w} \x1b[1;38;2;{};{};{}mbold\x1b[0m ", c, 255 - c, (c * 7) % 256).as_bytes());
        }
        v.extend_from_slice(b"\r\n");
    }
    v
}

/// Wide and astral characters: CJK, emoji, combining marks.
pub fn wide(lines: usize) -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..lines {
        v.extend_from_slice(format!("{i} 漢字かな交じり文 🚀✨🦀 café naïve e\u{301} 한국어 テスト\r\n").as_bytes());
    }
    v
}
