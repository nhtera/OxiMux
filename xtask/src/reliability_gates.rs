//! Validate `config/reliability-gates.toml` — the reliability claims ledger.
//!
//! Why this exists at all: OxiMux plan docs have twice carried a "VERIFIED" row
//! that was false. Once the evidence was a truncated `ls` that showed the first
//! screenful and was read as the whole set; once it was an API that genuinely
//! existed, just not on the host that mattered. Neither was a lie and neither
//! was caught in review, because prose has no shape a reader can check — a
//! sentence claiming coverage looks exactly like a sentence that has it.
//!
//! So the claim gets a shape. Every gate names the invariant, how you would
//! know it broke, and — the part that does the work — **where it must hold
//! versus where it is actually proven**. `platforms` is the claim,
//! `covered_platforms` is the evidence, and the gap between them is a field
//! rather than a silence. Both of the false rows above were exactly that gap
//! going unrecorded.
//!
//! What this checker deliberately does NOT do: assert that any test passes.
//! That is `cargo test`'s job and duplicating it here would make the ledger a
//! second, worse CI. This validates shape and internal consistency only. **An
//! uncovered platform is data, not a failure.** If that line ever blurs —
//! if contributors start deleting gates to get CI green — the mechanism has
//! failed and should be cut back rather than loosened.
//!
//! It is also a ratchet in the same spirit as `file-size-allow.txt`: a gate is
//! added when something breaks, not speculatively, and the honest thing to do
//! with a stale one is delete it.

use std::collections::BTreeSet;
use std::error::Error;
use std::path::Path;

use serde::Deserialize;

/// Repo-relative location of the ledger.
pub const LEDGER: &str = "config/reliability-gates.toml";

/// The only schema version this validator understands. Bumping it is a
/// deliberate act: old ledgers should fail loudly rather than be reinterpreted.
const SCHEMA_VERSION: u32 = 1;

/// Upper bound from the plan's own risk assessment. A large stale ledger is
/// worse than a small honest one, so growth past this is a prompt to cut back
/// to the gates that map to real incidents — not to raise the cap.
const SOFT_MAX_GATES: usize = 25;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    schema_version: u32,
    #[serde(default)]
    gate: Vec<Gate>,
}

/// How much the project currently trusts a gate's invariant.
///
/// An enum rather than a string so a typo (`stabel`) fails at parse instead of
/// quietly becoming a new status nobody ever queries.
#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
enum Status {
    /// Newly written; the invariant may still be wrong.
    Experimental,
    /// Believed correct, accumulating runtime before it is trusted.
    Soaking,
    /// Proven and relied upon.
    Stable,
    /// Known to fail intermittently. First-class here so a flaky test is
    /// tracked data with a recorded cause, rather than a code comment that
    /// the next person reads as folklore.
    Flaky,
    /// The invariant matters and is knowingly unproven. This is the status
    /// that keeps the ledger honest: without it, an uncovered area has to
    /// either lie or go unlisted.
    AcceptedGap,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Gate {
    /// Stable identifier, `area.short-claim`. Referenced from code comments
    /// and from phase docs, so renaming one is a breaking change.
    id: String,
    title: String,
    status: Status,
    /// The crate or subsystem that would fix this if it broke.
    owner: String,
    /// What must be true. Stated as a property, not as a test name — the test
    /// is evidence for the invariant, not the invariant itself.
    invariant: String,
    /// How a regression would present. The valuable half is usually the
    /// failure's *appearance*: the bugs that survive longest are the ones that
    /// look like something else.
    oracle: String,
    /// Where the invariant must hold.
    #[serde(default)]
    platforms: Vec<String>,
    /// Where it is actually proven. A subset of `platforms`.
    #[serde(default)]
    covered_platforms: Vec<String>,
    /// Which agents the invariant must hold for (empty when agent-agnostic).
    #[serde(default)]
    agents: Vec<String>,
    /// Which agents it is actually proven for. A subset of `agents`.
    #[serde(default)]
    covered_agents: Vec<String>,
    /// Prose for everything the fields above cannot carry — most importantly,
    /// *why* a gap exists and what would close it. A gate whose coverage is
    /// partial and whose notes are empty is not yet honest.
    #[serde(default)]
    coverage_notes: String,
    /// Repo-relative files holding the evidence. Existence is checked; content
    /// is not. May be empty for an `accepted-gap`.
    #[serde(default)]
    test_files: Vec<String>,
    /// Test function names within `test_files`. Recorded so a rename that
    /// silently drops coverage is visible in the diff of this file.
    #[serde(default)]
    assertions: Vec<String>,
}

/// Crate-level `cfg` attributes and the platforms they compile away on.
///
/// Deliberately only the crate-level (`#!`) form: a file gated wholesale is a
/// claim about the whole file, which is what a `test_files` entry is too. An
/// item-level `#[cfg]` inside a file says nothing about the file's other tests
/// and is left to the reader.
const CFG_EXCLUSIONS: &[(&str, &[&str])] = &[
    ("#![cfg(unix)]", &["windows"]),
    ("#![cfg(windows)]", &["macos", "linux"]),
    ("#![cfg(target_os = \"macos\")]", &["windows", "linux"]),
];

/// Whether `text` carries `attr` as a crate-level attribute.
///
/// Line-anchored rather than a bare `contains`, because a source file may
/// legitimately mention the attribute inside a string literal or a comment —
/// this module is itself such a file (see `CFG_EXCLUSIONS`), and a naive
/// substring match made it look `#![cfg(windows)]`-gated.
fn has_crate_attr(text: &str, attr: &str) -> bool {
    text.lines().any(|line| line.trim_start().starts_with(attr))
}

/// Whether `text` defines `name` as a function, matched to a word boundary.
///
/// `contains("fn foo")` also matches `fn foo_bar`, so a stale half-renamed
/// assertion would keep validating — the precise failure the assertion check
/// exists to catch.
fn defines_fn(text: &str, name: &str) -> bool {
    text.match_indices(&format!("fn {name}"))
        .any(|(i, m)| {
            // The character after the name must end the identifier.
            !text[i + m.len()..]
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
}

impl Gate {
    /// Whether anything declared is not also proven.
    ///
    /// Set-wise on purpose: a length comparison would call
    /// `platforms = ["macos", "linux"]` with `covered = ["macos", "macos"]`
    /// fully covered.
    fn has_gap(&self) -> bool {
        self.platforms
            .iter()
            .any(|p| !self.covered_platforms.contains(p))
            || self.agents.iter().any(|a| !self.covered_agents.contains(a))
    }
}

pub fn run(root: &Path) -> Result<(), Box<dyn Error>> {
    let path = root.join(LEDGER);
    let text = std::fs::read_to_string(&path).map_err(|e| {
        format!(
            "cannot read {LEDGER} ({e}) — the ledger is committed, so a missing \
             file means a bad path or a deleted file, not an empty ledger"
        )
    })?;

    let ledger: Ledger =
        toml::from_str(&text).map_err(|e| format!("{LEDGER}: {e}"))?;

    let mut violations = validate(&ledger, root);

    // Report every problem in one run. A validator that stops at the first
    // makes fixing a ten-gate ledger a ten-round trip.
    if !violations.is_empty() {
        violations.sort();
        for v in &violations {
            eprintln!("FAIL  {v}");
        }
        return Err(format!(
            "reliability-gates: {} violation(s) in {LEDGER}",
            violations.len()
        )
        .into());
    }

    if ledger.gate.len() > SOFT_MAX_GATES {
        eprintln!(
            "WARN  {} gates (soft cap {SOFT_MAX_GATES}) — prefer cutting back to \
             the gates that map to real incidents over growing toward coverage",
            ledger.gate.len()
        );
    }

    let gaps = ledger.gate.iter().filter(|g| g.has_gap()).count();
    println!(
        "reliability-gates: ok ({} gates, {gaps} with a declared coverage gap)",
        ledger.gate.len()
    );
    Ok(())
}

/// Shape and internal-consistency checks. Never asserts that a test passes.
fn validate(ledger: &Ledger, root: &Path) -> Vec<String> {
    let mut out = Vec::new();

    if ledger.schema_version != SCHEMA_VERSION {
        out.push(format!(
            "schema_version is {} but this xtask understands {SCHEMA_VERSION}",
            ledger.schema_version
        ));
    }

    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for gate in &ledger.gate {
        let id = gate.id.as_str();

        if !seen.insert(id) {
            out.push(format!("duplicate gate id '{id}'"));
        }

        // Empty prose defeats the entire mechanism: a gate with no invariant
        // is a name, and a gate with no oracle cannot tell you it broke.
        for (field, value) in [
            ("id", &gate.id),
            ("title", &gate.title),
            ("owner", &gate.owner),
            ("invariant", &gate.invariant),
            ("oracle", &gate.oracle),
        ] {
            if value.trim().is_empty() {
                out.push(format!("gate '{id}': '{field}' is empty"));
            }
        }

        // The phase this ledger comes from puts it plainly: "Silently omitting
        // the distinction is the error." An empty `platforms` made every
        // downstream check vacuous — no gap could be computed, so no notes were
        // required — which let a gate say nothing about coverage and still
        // validate clean. An `accepted-gap` still declares where the invariant
        // must hold; it just leaves `covered_platforms` empty.
        if gate.platforms.is_empty() {
            out.push(format!(
                "gate '{id}': 'platforms' is empty — say where the invariant must \
                 hold, even when nothing proves it yet"
            ));
        }

        let mut evidence = String::new();
        for file in &gate.test_files {
            let path = root.join(file);
            if !path.exists() {
                out.push(format!(
                    "gate '{id}': test_files entry '{file}' does not exist on disk"
                ));
                continue;
            }
            // Concatenated so an assertion can live in any of the cited files;
            // which one holds it is not something the ledger should pin down.
            if let Ok(text) = std::fs::read_to_string(&path) {
                evidence.push_str(&text);
            }
        }

        // A named assertion that no longer exists is the quiet way coverage
        // disappears: the test is renamed or deleted, every other check still
        // passes, and the gate goes on claiming evidence that is gone.
        //
        // Matched to a word boundary, not by substring. `contains("fn foo")`
        // would let `foo` stand in for `foo_and_then_some` — a truncated or
        // half-renamed name still "proving" coverage, which is the same
        // defect this rule exists to catch.
        for assertion in &gate.assertions {
            if !defines_fn(&evidence, assertion) {
                out.push(format!(
                    "gate '{id}': assertion '{assertion}' is not defined in any of \
                     its test_files — renamed, deleted, or moved"
                ));
            }
        }

        // A `#![cfg(unix)]` at the top of a cited file means it compiles to
        // nothing on Windows, so a `covered_platforms` naming Windows is false
        // however green CI looks. This rule exists because exactly that slipped
        // into the seed ledger: a gate claimed Windows coverage from a file
        // whose own header says the invariant does not hold there.
        for file in &gate.test_files {
            let Ok(text) = std::fs::read_to_string(root.join(file)) else {
                continue;
            };
            for (attr, excluded) in CFG_EXCLUSIONS {
                if !has_crate_attr(&text, attr) {
                    continue;
                }
                for platform in *excluded {
                    if gate.covered_platforms.iter().any(|p| p == platform) {
                        out.push(format!(
                            "gate '{id}': covered_platforms claims '{platform}', but \
                             '{file}' is `{attr}` and compiles to nothing there"
                        ));
                    }
                }
            }
        }

        // The core check. A `covered_*` value outside its declared list means
        // the two lists have drifted, and the gap — the thing this ledger
        // exists to make visible — can no longer be read off them.
        for covered in &gate.covered_platforms {
            if !gate.platforms.contains(covered) {
                out.push(format!(
                    "gate '{id}': covered_platforms has '{covered}', which is not in platforms {:?}",
                    gate.platforms
                ));
            }
        }
        for covered in &gate.covered_agents {
            if !gate.agents.contains(covered) {
                out.push(format!(
                    "gate '{id}': covered_agents has '{covered}', which is not in agents {:?}",
                    gate.agents
                ));
            }
        }

        // A partial claim with no explanation is the exact shape of the two
        // false "VERIFIED" rows: the gap existed, nobody wrote down why.
        //
        // Asked set-wise rather than by comparing lengths. A duplicate in
        // `covered_platforms` would make the counts agree while a declared
        // platform went unproven — a false coverage claim passing through the
        // mechanism built to catch false coverage claims.
        let has_gap = gate.has_gap();
        if has_gap && gate.coverage_notes.trim().is_empty() {
            out.push(format!(
                "gate '{id}': declares a coverage gap but 'coverage_notes' is empty — \
                 say why it is uncovered and what would close it"
            ));
        }

        // `flaky` without a recorded cause is the code comment this field
        // exists to replace.
        if gate.status == Status::Flaky && gate.coverage_notes.trim().is_empty() {
            out.push(format!(
                "gate '{id}': status is 'flaky' but 'coverage_notes' is empty — \
                 record the symptom and the known cause"
            ));
        }

        // Anything other than an accepted gap is claiming evidence; make it
        // point at some.
        if gate.test_files.is_empty() && gate.status != Status::AcceptedGap {
            out.push(format!(
                "gate '{id}': no test_files, but status is not 'accepted-gap' — \
                 either cite the evidence or say the gap is accepted"
            ));
        }

        // `stable` means "proven and relied upon", so a stable gate proven
        // nowhere is a contradiction in its own row. Citing `test_files` is not
        // enough — a check that exists but runs on no platform gates nothing,
        // which is the state `data-dir-lint` was in when this rule was written.
        if gate.status == Status::Stable
            && gate.covered_platforms.is_empty()
            && gate.covered_agents.is_empty()
        {
            out.push(format!(
                "gate '{id}': status is 'stable' but nothing is covered — a claim \
                 proven nowhere is 'accepted-gap', not 'stable'"
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a ledger body and validate it against `root`, returning the
    /// violation list. Parse errors are surfaced as a single pseudo-violation
    /// so a test can assert on them the same way.
    fn check(body: &str, root: &Path) -> Vec<String> {
        match toml::from_str::<Ledger>(body) {
            Ok(l) => validate(&l, root),
            Err(e) => vec![format!("parse: {e}")],
        }
    }

    /// A gate with every required field filled, to be broken one field at a
    /// time by the tests below.
    fn good_gate(id: &str, test_file: &str) -> String {
        format!(
            r#"
[[gate]]
id = "{id}"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos"]
covered_platforms = ["macos"]
test_files = ["{test_file}"]
"#
        )
    }

    /// A file guaranteed to exist relative to the repo root, so `test_files`
    /// existence checks pass without a fixture directory.
    const REAL_FILE: &str = "Cargo.toml";

    fn root() -> std::path::PathBuf {
        // The crate dir is `<root>/xtask`.
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent")
            .to_path_buf()
    }

    #[test]
    fn a_well_formed_ledger_has_no_violations() {
        let body = format!("schema_version = 1\n{}", good_gate("a.b", REAL_FILE));
        assert!(check(&body, &root()).is_empty());
    }

    /// The headline behaviour: one run reports every problem. A first-error
    /// validator turns a ten-gate fix into ten CI rounds, so this asserts the
    /// count, not just that something failed.
    #[test]
    fn every_violation_class_is_reported_in_a_single_run() {
        let body = format!(
            r#"schema_version = 9
{}{}
[[gate]]
id = "dup.id"
title = "T"
status = "stable"
owner = "o"
invariant = "   "
oracle = "o"
platforms = ["macos"]
covered_platforms = ["windows"]
agents = ["claude"]
covered_agents = ["codex"]
test_files = ["no/such/file.rs"]
"#,
            good_gate("dup.id", REAL_FILE),
            good_gate("other.gate", REAL_FILE),
        );
        let v = check(&body, &root());

        let has = |needle: &str| v.iter().any(|s| s.contains(needle));
        assert!(has("schema_version is 9"), "{v:?}");
        assert!(has("duplicate gate id 'dup.id'"), "{v:?}");
        assert!(has("'invariant' is empty"), "{v:?}");
        assert!(has("does not exist on disk"), "{v:?}");
        assert!(has("covered_platforms has 'windows'"), "{v:?}");
        assert!(has("covered_agents has 'codex'"), "{v:?}");
        // Six distinct classes, all from one pass.
        assert!(v.len() >= 6, "expected every class at once, got {v:?}");
    }

    /// The central contract: a declared-but-uncovered platform is legal. If
    /// this ever starts failing, the ledger has become a coverage mandate and
    /// contributors will delete gates to get CI green.
    #[test]
    fn a_declared_coverage_gap_is_data_not_a_violation() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos", "windows"]
covered_platforms = ["macos"]
coverage_notes = "windows path compiles but has no assertion yet"
test_files = ["{REAL_FILE}"]
"#
        );
        assert!(check(&body, &root()).is_empty());
    }

    #[test]
    fn a_coverage_gap_without_notes_is_a_violation() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos", "windows"]
covered_platforms = ["macos"]
test_files = ["{REAL_FILE}"]
"#
        );
        let v = check(&body, &root());
        assert!(
            v.iter().any(|s| s.contains("coverage_notes' is empty")),
            "{v:?}"
        );
    }

    /// A duplicate in `covered_platforms` must not pass as coverage of a
    /// different platform. Counting instead of comparing sets would let a
    /// false coverage claim through the check that exists to catch exactly
    /// that, so this is pinned rather than left to review.
    #[test]
    fn a_duplicate_does_not_stand_in_for_an_uncovered_platform() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos", "linux"]
covered_platforms = ["macos", "macos"]
test_files = ["{REAL_FILE}"]
"#
        );
        let v = check(&body, &root());
        assert!(
            v.iter().any(|s| s.contains("coverage_notes' is empty")),
            "a duplicate must not pass as coverage of linux; got {v:?}"
        );
    }

    #[test]
    fn flaky_without_a_recorded_cause_is_a_violation() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "flaky"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos"]
covered_platforms = ["macos"]
test_files = ["{REAL_FILE}"]
"#
        );
        let v = check(&body, &root());
        assert!(v.iter().any(|s| s.contains("status is 'flaky'")), "{v:?}");
    }

    #[test]
    fn an_accepted_gap_may_cite_no_tests_but_a_stable_gate_may_not() {
        let accepted = r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "accepted-gap"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos"]
coverage_notes = "no harness for this yet"
"#;
        assert!(check(accepted, &root()).is_empty());

        let stable = accepted.replace("accepted-gap", "stable");
        let v = check(&stable, &root());
        assert!(v.iter().any(|s| s.contains("no test_files")), "{v:?}");
    }

    /// `deny_unknown_fields` and the `status` enum are most of the value: a
    /// renamed field or a typo'd status must fail at parse, not be ignored.
    #[test]
    fn unknown_fields_and_bad_statuses_fail_at_parse() {
        let unknown = format!(
            "schema_version = 1\n{}covered_platfroms = [\"macos\"]\n",
            good_gate("a.b", REAL_FILE)
        );
        let v = check(&unknown, &root());
        assert!(v.iter().any(|s| s.starts_with("parse:")), "{v:?}");

        let typo = format!("schema_version = 1\n{}", good_gate("a.b", REAL_FILE))
            .replace(r#"status = "stable""#, r#"status = "stabel""#);
        let v = check(&typo, &root());
        assert!(v.iter().any(|s| s.starts_with("parse:")), "{v:?}");
    }

    /// Coverage disappears quietly: a test is renamed, everything else still
    /// passes, and the gate keeps citing evidence that no longer exists.
    #[test]
    fn an_assertion_that_no_longer_exists_is_a_violation() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos"]
covered_platforms = ["macos"]
test_files = ["xtask/src/reliability_gates.rs"]
assertions = ["{}"]
"#,
            "no_such_test_function_exists"
        );
        let v = check(&body, &root());
        assert!(
            v.iter().any(|s| s.contains("is not defined in any of")),
            "{v:?}"
        );

        // And the same gate passes once it names a real one — otherwise this
        // test would also pass against a checker that rejects everything.
        let ok = body.replace(
            "no_such_test_function_exists",
            "an_assertion_that_no_longer_exists_is_a_violation",
        );
        assert!(check(&ok, &root()).is_empty(), "{:?}", check(&ok, &root()));
    }

    /// A file gated `#![cfg(unix)]` compiles to nothing on Windows, so a gate
    /// citing it cannot claim Windows however green CI looks. This rule was
    /// added because exactly that claim reached the seed ledger — from a file
    /// whose own header says the invariant does not hold on Windows.
    #[test]
    fn a_cfg_gated_file_cannot_prove_the_platform_it_excludes() {
        let body = r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos", "windows"]
covered_platforms = ["macos", "windows"]
coverage_notes = "n"
test_files = ["crates/computer-use/tests/install_swap_semantics.rs"]
"#;
        let v = check(body, &root());
        assert!(
            v.iter()
                .any(|s| s.contains("is `#![cfg(unix)]` and compiles to nothing")),
            "{v:?}"
        );

        // Dropping the false platform clears it — so the rule is about the
        // claim, not about the file being cited at all.
        let ok = body.replace(
            r#"covered_platforms = ["macos", "windows"]"#,
            r#"covered_platforms = ["macos"]"#,
        );
        assert!(check(&ok, &root()).is_empty(), "{:?}", check(&ok, &root()));
    }

    /// `contains("fn foo")` also matches `fn foo_bar`, so a truncated or
    /// half-renamed assertion would keep validating — the same defect the
    /// assertion check exists to catch.
    #[test]
    fn an_assertion_name_must_match_to_a_word_boundary() {
        let text = "fn a_well_formed_ledger_has_no_violations() {}";
        assert!(defines_fn(text, "a_well_formed_ledger_has_no_violations"));
        assert!(!defines_fn(text, "a_well_formed_ledger"));
        assert!(!defines_fn(text, "a_well_formed_ledger_has_no_violation"));
    }

    /// The phase is explicit that silently omitting the platform distinction is
    /// the error. An empty `platforms` made every downstream check vacuous.
    #[test]
    fn a_gate_may_not_omit_the_platform_distinction() {
        let body = r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "accepted-gap"
owner = "o"
invariant = "i"
oracle = "o"
coverage_notes = "n"
"#;
        let v = check(body, &root());
        assert!(v.iter().any(|s| s.contains("'platforms' is empty")), "{v:?}");
    }

    /// `stable` means "proven and relied upon", so a stable gate proven nowhere
    /// contradicts itself. Written after the seed ledger carried exactly that:
    /// a lint that passes locally, gates nothing in CI, and was marked stable.
    #[test]
    fn a_stable_gate_proven_nowhere_is_a_violation() {
        let body = format!(
            r#"schema_version = 1

[[gate]]
id = "a.b"
title = "T"
status = "stable"
owner = "o"
invariant = "i"
oracle = "o"
platforms = ["macos"]
covered_platforms = []
coverage_notes = "runs nowhere"
test_files = ["{REAL_FILE}"]
"#
        );
        let v = check(&body, &root());
        assert!(
            v.iter().any(|s| s.contains("nothing is covered")),
            "{v:?}"
        );

        // The same row is legal once it stops claiming to be proven.
        let ok = body.replace(r#"status = "stable""#, r#"status = "accepted-gap""#);
        assert!(check(&ok, &root()).is_empty(), "{:?}", check(&ok, &root()));
    }

    /// The committed ledger must itself be valid — otherwise the check only
    /// ever runs against fixtures and the real file rots.
    #[test]
    fn the_committed_ledger_validates() {
        run(&root()).expect("config/reliability-gates.toml must validate");
    }
}
