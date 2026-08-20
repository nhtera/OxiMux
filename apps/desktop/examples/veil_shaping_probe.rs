//! Does splitting a text run by **colour alone** change how the text is shaped?
//!
//! Phase 7's streaming fade veil rests on the claim that it does not: the veil
//! paints newly-arrived characters in a fading colour, and that is only safe if
//! a colour boundary is invisible to layout. If a colour split moved a wrap
//! point, the veil would reflow text as it faded — worse than no veil at all.
//!
//! The claim cannot be checked with a `#[gpui::test]`. GPUI's test platform
//! installs `NoopTextSystem`, whose `layout_line` ignores its `runs` argument
//! entirely and advances every character by a fixed em width, so a colour split
//! is *by construction* invisible there — such a test would pass whether or not
//! the real shaper agreed. This probe boots a headless **real** platform and
//! asks the real text system instead.
//!
//! Two details of the pinned rev make the question worth asking rather than
//! assuming. `WindowTextSystem::shape_text` starts a new `FontRun` whenever the
//! decoration changes, and colour is part of the decoration — so a colour split
//! *is* a font-run split by the time the shaper sees it. And `MacTextSystem`
//! deliberately alternates each run's font size by one ULP to stop CoreText
//! forming ligatures across a run boundary. Colour therefore reaches the shaper
//! by two separate paths, neither of which is obviously layout-neutral.
//!
//! ```sh
//! cargo run -p oximux-app --example veil_shaping_probe
//! ```

use gpui::{App, Font, FontFeatures, Hsla, Pixels, SharedString, TextRun, WindowTextSystem, hsla, px};

/// One shaping question: the same string, shaped as one run and then as several
/// colour-split runs, at the same wrap width.
struct Case {
    what: &'static str,
    text: &'static str,
    /// Byte offsets to split at — deliberately mid-word and mid-ligature.
    splits: &'static [usize],
    wrap: f32,
}

const CASES: &[Case] = &[
    Case {
        what: "prose, split mid-word",
        text: "The quick brown fox jumps over the lazy dog while the \
               transcript keeps streaming one token at a time.",
        splits: &[20, 47, 61],
        wrap: 220.0,
    },
    Case {
        what: "split between the letters of a ligature pair",
        text: "office affluent fluffy difficult",
        splits: &[2, 9, 17, 24],
        wrap: 400.0,
    },
    Case {
        what: "one character split off the end",
        text: "a streaming reply grows one character at a time like this",
        splits: &[56],
        wrap: 180.0,
    },
];

/// The veil boundary walks forward as tokens arrive, so no single split offset
/// is representative. This line is swept one byte at a time.
const SWEEP: &str = "difficult offices affix fluffy waffle scaffolding";

fn main() {
    gpui_platform::headless().run(|cx: &mut App| {
        let ts = WindowTextSystem::new(cx.text_system().clone());
        let size = px(14.0);
        let mut reflow = false;
        let mut ligature_broken = false;
        // Widest line-width difference seen anywhere, and the case that produced
        // it. One unit throughout: mixing ulps and pixels across different cases
        // makes the numbers look incomparable when they are not.
        let mut worst: (f32, String) = (0.0, "none".into());

        // The families OxiMux actually ships on macOS: `platform_fonts::UI`
        // ("Helvetica Neue", falling back to "Helvetica") for prose and
        // `platform_fonts::MONO` ("Menlo") for code. `.SystemUIFont` is here as
        // a reference point, not because chat uses it.
        for family in ["Helvetica Neue", "Helvetica", "Menlo", ".SystemUIFont"] {
            let font = Font {
                family: SharedString::from(family),
                features: FontFeatures::default(),
                fallbacks: None,
                weight: Default::default(),
                style: Default::default(),
            };
            println!("\n=== {family} @ {size:?} ===");

            for case in CASES {
                let whole = vec![run(&font, case.text.len(), hsla(0.0, 0.0, 0.9, 1.0))];
                let split = split_runs(&font, case.text, case.splits);
                let a = shape(&ts, case.text, size, &whole, case.wrap);
                let b = shape(&ts, case.text, size, &split, case.wrap);

                // Three axes, most consequential first.
                let moved = a.wraps != b.wraps;
                let delta = (a.width - b.width).abs();
                let reglyphed = a.glyphs != b.glyphs;
                reflow |= moved;
                ligature_broken |= reglyphed;
                if delta > worst.0 {
                    worst = (delta, format!("{family}: {}", case.what));
                }

                println!(
                    "  {:<45} 1->{} runs  wraps {}  width {}  glyphs {}",
                    case.what,
                    split.len(),
                    if moved { "MOVED" } else { "same " },
                    width_note(delta),
                    if reglyphed {
                        format!("{} -> {} BROKEN", a.glyphs, b.glyphs)
                    } else {
                        format!("{} same", a.glyphs)
                    },
                );
            }

            let whole = vec![run(&font, SWEEP.len(), hsla(0.0, 0.0, 0.9, 1.0))];
            let want = shape(&ts, SWEEP, size, &whole, 260.0);
            let (mut moved, mut broke) = (0usize, 0usize);
            let mut worst_px = 0.0f32;
            for at in 1..SWEEP.len() {
                let got = shape(&ts, SWEEP, size, &split_runs(&font, SWEEP, &[at]), 260.0);
                worst_px = worst_px.max((got.width - want.width).abs());
                if got.wraps != want.wraps {
                    if moved == 0 {
                        println!(
                            "    first reflow at byte {at} (…{}|{}…): wraps {:?} -> {:?}",
                            &SWEEP[at.saturating_sub(4)..at],
                            &SWEEP[at..(at + 4).min(SWEEP.len())],
                            want.wraps,
                            got.wraps
                        );
                    }
                    moved += 1;
                    reflow = true;
                }
                if got.glyphs != want.glyphs {
                    broke += 1;
                    ligature_broken = true;
                }
            }
            if worst_px > worst.0 {
                worst = (worst_px, format!("{family}: swept boundary"));
            }
            println!(
                "  {:<45} {moved} of {} positions reflow, {broke} break a ligature, worst width {}",
                "veil boundary swept over every character",
                SWEEP.len() - 1,
                width_note(worst_px),
            );
        }

        println!("\n--- verdict ---");
        println!(
            "wrap points move under a colour split : {}",
            if reflow {
                "YES — the veil would reflow text"
            } else {
                "no"
            }
        );
        println!(
            "largest line-width difference         : {} ({})",
            width_note(worst.0),
            worst.1
        );
        println!(
            "ligatures survive a colour split      : {}",
            if ligature_broken {
                "NO — a split inside a ligature pair breaks it"
            } else {
                "yes"
            }
        );
        cx.quit();
    });
}

/// What "the layout" means here: how wide the unwrapped line is, where it
/// wrapped, and how many glyphs it took. A reflow moves the first two; a broken
/// ligature moves the third.
#[derive(PartialEq, Debug)]
struct Shape {
    width: f32,
    wraps: Vec<usize>,
    glyphs: usize,
}

fn shape(ts: &WindowTextSystem, text: &str, size: Pixels, runs: &[TextRun], wrap: f32) -> Shape {
    let lines = ts
        .shape_text(
            SharedString::from(text.to_string()),
            size,
            runs,
            Some(px(wrap)),
            None,
        )
        .expect("shape_text");
    let line = &lines[0];
    Shape {
        width: f32::from(line.unwrapped_layout.width),
        wraps: line.wrap_boundaries.iter().map(|b| b.glyph_ix).collect(),
        glyphs: line
            .unwrapped_layout
            .runs
            .iter()
            .map(|r| r.glyphs.len())
            .sum(),
    }
}

/// Splitting a run makes the shaper do the same arithmetic in a different order,
/// so a difference of a few thousandths of a pixel is float noise rather than a
/// layout change. Anything a reader could see is far larger than that.
fn width_note(delta: f32) -> String {
    if delta < 0.001 {
        "identical".into()
    } else {
        format!("{delta:+.4}px")
    }
}

fn run(font: &Font, len: usize, color: Hsla) -> TextRun {
    TextRun {
        len,
        font: font.clone(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// The same text cut at `splits`, each piece a slightly different colour —
/// exactly the shape a fade veil produces.
fn split_runs(font: &Font, text: &str, splits: &[usize]) -> Vec<TextRun> {
    let mut runs = Vec::new();
    let mut at = 0;
    for (i, &to) in splits.iter().chain(std::iter::once(&text.len())).enumerate() {
        if to > at {
            runs.push(run(font, to - at, hsla(0.0, 0.0, 0.9, 1.0 - i as f32 * 0.1)));
            at = to;
        }
    }
    runs
}
