//! The signing teams this Mac can build the control runner with.
//!
//! `security find-identity -v -p codesigning` lists the valid identities
//! that have a private key; the development ones ("Apple Development", or the
//! older "iPhone Developer") are kept. A certificate's subject names its team:
//! `OU` is the team id, `O` the team's name — read from the certificate
//! (`security find-certificate -a -Z -p`) with a minimal DER walk.
//!
//! OxiMux never picks a team by itself: the user chooses one (stored by the
//! app in its settings database), and the id is checked with
//! [`valid_team_id`] before it reaches an `xcodebuild` argument.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use base64::Engine as _;

use crate::Result;
use crate::runner::Runner;

/// Identities that can sign an app for a phone during development.
const DEVELOPMENT: &[&str] = &["Apple Development:", "iPhone Developer:"];

/// A team a development certificate on this Mac belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Team {
    /// The ten-character team id (the certificate's `OU`).
    pub id: String,
    /// The team's name (the certificate's `O`).
    pub name: String,
    /// A free Personal Team (its certificate is named for an Apple ID's
    /// email address): it can sign for the user's own phones only, and its
    /// profiles last a week.
    pub personal: bool,
}

/// A team id as Apple issues them: ten upper-case letters or digits.
pub fn valid_team_id(id: &str) -> bool {
    id.len() == 10 && id.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// The teams of this Mac's valid development identities, by name.
pub fn teams(runner: &dyn Runner, timeout: Duration) -> Result<Vec<Team>> {
    let out = runner.run("security", &["find-identity", "-v", "-p", "codesigning"], None, timeout)?;
    let identities = development_identities(&out.into_success("security find-identity")?.stdout_str());
    if identities.is_empty() {
        return Ok(Vec::new());
    }
    let out = runner.run("security", &["find-certificate", "-a", "-Z", "-p"], None, timeout)?;
    let certificates = certificates_by_sha1(&out.into_success("security find-certificate")?.stdout_str());
    let mut teams = BTreeMap::new();
    for sha1 in identities {
        let Some(team) = certificates.get(&sha1).and_then(|der| team_of(der)) else { continue };
        // A team with two identities on this Mac is one choice.
        teams.entry(team.id.clone()).or_insert(team);
    }
    let mut teams: Vec<Team> = teams.into_values().collect();
    teams.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()).then(a.id.cmp(&b.id)));
    Ok(teams)
}

/// The SHA-1s of the development identities in `find-identity` output:
/// lines like `  1) <40 hex> "Apple Development: Name (ABCDE12345)"`.
fn development_identities(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.trim_start().split_once(") ")?;
            let (sha1, name) = rest.split_once(' ')?;
            let name = name.trim().trim_matches('"');
            let ok = sha1.len() == 40 && sha1.bytes().all(|b| b.is_ascii_hexdigit()) && DEVELOPMENT.iter().any(|p| name.starts_with(p));
            ok.then(|| sha1.to_ascii_uppercase())
        })
        .collect()
}

/// Each certificate's DER, by the SHA-1 `find-certificate -Z` prints above it.
fn certificates_by_sha1(output: &str) -> HashMap<String, Vec<u8>> {
    let mut found = HashMap::new();
    let mut sha1: Option<String> = None;
    let mut pem: Option<String> = None;
    for line in output.lines() {
        let line = line.trim();
        if let Some(hash) = line.strip_prefix("SHA-1 hash:") {
            sha1 = Some(hash.trim().to_ascii_uppercase());
        } else if line == "-----BEGIN CERTIFICATE-----" {
            pem = Some(String::new());
        } else if line == "-----END CERTIFICATE-----" {
            let body = pem.take().unwrap_or_default();
            if let (Some(hash), Ok(der)) = (sha1.take(), base64::engine::general_purpose::STANDARD.decode(body)) {
                found.insert(hash, der);
            }
        } else if let Some(body) = pem.as_mut() {
            body.push_str(line);
        }
    }
    found
}

/// The team a certificate's subject names, when it names a valid one.
fn team_of(der: &[u8]) -> Option<Team> {
    let subject = subject(der)?;
    let id = subject.get(&OU)?.clone();
    if !valid_team_id(&id) {
        return None;
    }
    let name = subject.get(&O).cloned().unwrap_or_else(|| id.clone());
    let personal = subject.get(&CN).is_some_and(|cn| cn.contains('@'));
    Some(Team { id, name, personal })
}

const CN: [u8; 3] = [0x55, 0x04, 0x03];
const O: [u8; 3] = [0x55, 0x04, 0x0A];
const OU: [u8; 3] = [0x55, 0x04, 0x0B];

/// A certificate's subject attributes (by OID) — `Certificate ::= SEQUENCE {
/// tbsCertificate SEQUENCE { [0] version?, serial, signature, issuer,
/// validity, subject, … }, … }`.
fn subject(der: &[u8]) -> Option<HashMap<[u8; 3], String>> {
    let (certificate, _) = tlv(der, 0x30)?;
    let (tbs, _) = tlv(certificate, 0x30)?;
    let mut rest = tbs;
    if rest.first() == Some(&0xA0) {
        rest = skip(rest)?;
    }
    for _ in 0..4 {
        // serial, signature, issuer, validity
        rest = skip(rest)?;
    }
    let (mut name, _) = tlv(rest, 0x30)?;
    let mut attributes = HashMap::new();
    // Name ::= SEQUENCE OF SET OF SEQUENCE { OID, value }
    while !name.is_empty() {
        let (set, after) = tlv(name, 0x31)?;
        name = after;
        let mut set = set;
        while !set.is_empty() {
            let (pair, after) = tlv(set, 0x30)?;
            set = after;
            let (oid, value) = tlv(pair, 0x06)?;
            let (&tag, _) = value.split_first()?;
            let (text, _) = tlv(value, tag)?;
            if let (Ok(oid), Ok(text)) = (<[u8; 3]>::try_from(oid), std::str::from_utf8(text)) {
                attributes.insert(oid, text.to_owned());
            }
        }
    }
    Some(attributes)
}

/// The contents of the element at the start of `der` with tag `tag`, and
/// what follows it.
fn tlv(der: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (&found, rest) = der.split_first()?;
    if found != tag {
        return None;
    }
    let (&first, mut rest) = rest.split_first()?;
    let length = if first < 0x80 {
        usize::from(first)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 || rest.len() < count {
            return None;
        }
        let (bytes, after) = rest.split_at(count);
        rest = after;
        bytes.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b))
    };
    (rest.len() >= length).then(|| rest.split_at(length))
}

/// What follows the element at the start of `der`, whatever its tag.
fn skip(der: &[u8]) -> Option<&[u8]> {
    let tag = *der.first()?;
    tlv(der, tag).map(|(_, rest)| rest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::{CmdOutput, ScriptedRunner, SystemRunner};

    fn element(tag: u8, contents: &[u8]) -> Vec<u8> {
        let mut out = vec![tag];
        if contents.len() < 0x80 {
            out.push(contents.len() as u8);
        } else {
            out.extend([0x82, (contents.len() >> 8) as u8, contents.len() as u8]);
        }
        out.extend_from_slice(contents);
        out
    }

    fn name(attributes: &[([u8; 3], u8, &str)]) -> Vec<u8> {
        let sets: Vec<u8> = attributes
            .iter()
            .flat_map(|(oid, tag, value)| {
                let pair = [element(0x06, oid), element(*tag, value.as_bytes())].concat();
                element(0x31, &element(0x30, &pair))
            })
            .collect();
        element(0x30, &sets)
    }

    /// A certificate shaped like Apple's, signature and key left out.
    fn certificate(subject: &[([u8; 3], u8, &str)]) -> Vec<u8> {
        let tbs = [
            element(0xA0, &element(0x02, &[2])),
            element(0x02, &[0x01, 0x23]),
            element(0x30, &element(0x06, &[0x2A, 0x86, 0x48])),
            name(&[(CN, 0x0C, "Apple Worldwide Developer Relations Certification Authority")]),
            element(0x30, &[element(0x17, b"260101000000Z"), element(0x17, b"270101000000Z")].concat()),
            name(subject),
            element(0x30, &[0u8; 200]),
        ]
        .concat();
        element(0x30, &element(0x30, &tbs))
    }

    fn pem(sha1: &str, der: &[u8]) -> String {
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        let lines: Vec<&str> = body.as_bytes().chunks(64).map(|c| std::str::from_utf8(c).unwrap()).collect();
        format!("SHA-256 hash: {}\nSHA-1 hash: {sha1}\n-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----\n", "0".repeat(64), lines.join("\n"))
    }

    const A: &str = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    const B: &str = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
    const C: &str = "CCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCCC";
    const D: &str = "DDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDDD";

    #[test]
    fn team_ids_are_ten_upper_case_letters_or_digits() {
        assert!(valid_team_id("QCLX0D7V9M"));
        for bad in ["qclx0d7v9m", "QCLX0D7V9", "QCLX0D7V9MM", "QCLX0D7V9-", "QCLX0D7V9M\n", "$(rm -rf)"] {
            assert!(!valid_team_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn development_identities_are_kept_and_others_dropped() {
        let output = format!(
            "  1) {A} \"Apple Development: Jo Doe (ABCDE12345)\"\n  2) {B} \"Developer ID Application: Jo Doe (TEAM000001)\"\n  3) {C} \"iPhone Developer: jo@example.com (FGHIJ67890)\"\n  4) {D} \"Some Self-Signed\"\n     4 valid identities found\n"
        );
        assert_eq!(development_identities(&output), vec![A.to_owned(), C.to_owned()]);
    }

    #[test]
    fn a_subjects_team_is_read_from_the_certificate() {
        let der = certificate(&[
            ([0x09, 0x92, 0x26], 0x0C, "UID-not-ours"),
            (CN, 0x0C, "Apple Development: Jo Doe (ABCDE12345)"),
            (OU, 0x0C, "TEAM000001"),
            (O, 0x0C, "Jo Doe"),
            ([0x55, 0x04, 0x06], 0x13, "US"),
        ]);
        assert_eq!(team_of(&der), Some(Team { id: "TEAM000001".into(), name: "Jo Doe".into(), personal: false }));
        let personal = certificate(&[(CN, 0x0C, "Apple Development: jo@example.com (FGHIJ67890)"), (OU, 0x13, "PERS000002"), (O, 0x0C, "jo doe")]);
        assert!(team_of(&personal).unwrap().personal);
        // No team, or not a team id: not offered.
        assert_eq!(team_of(&certificate(&[(CN, 0x0C, "x")])), None);
        assert_eq!(team_of(&certificate(&[(OU, 0x0C, "not a team")])), None);
        // Truncated input is no team, never a panic.
        for cut in 0..der.len() {
            let _ = team_of(&der[..cut]);
        }
    }

    #[test]
    fn teams_pair_identities_with_their_certificates() {
        let identities = format!(
            "  1) {A} \"Apple Development: Jo Doe (ABCDE12345)\"\n  2) {B} \"Apple Development: jo@example.com (FGHIJ67890)\"\n  3) {C} \"Apple Development: Jo Doe (ZZZZZ99999)\"\n     3 valid identities found\n"
        );
        let team = certificate(&[(CN, 0x0C, "Apple Development: Jo Doe (ABCDE12345)"), (OU, 0x0C, "TEAM000001"), (O, 0x0C, "Zeta Labs")]);
        let personal = certificate(&[(CN, 0x0C, "Apple Development: jo@example.com (FGHIJ67890)"), (OU, 0x0C, "PERS000002"), (O, 0x0C, "jo doe")]);
        let certificates = [pem(A, &team), pem(B, &personal), pem(C, &team), pem(D, &team)].concat();
        let runner = ScriptedRunner::new([])
            .expect("security find-identity -v -p codesigning", CmdOutput::ok(identities))
            .expect("security find-certificate -a -Z -p", CmdOutput::ok(certificates));
        let teams = teams(&runner, Duration::from_secs(5)).unwrap();
        assert_eq!(
            teams,
            vec![
                Team { id: "PERS000002".into(), name: "jo doe".into(), personal: true },
                Team { id: "TEAM000001".into(), name: "Zeta Labs".into(), personal: false },
            ]
        );
    }

    #[test]
    fn no_development_identity_means_no_teams_and_no_second_call() {
        let runner = ScriptedRunner::new([]).expect("security find-identity -v -p codesigning", CmdOutput::ok("     0 valid identities found\n"));
        assert_eq!(teams(&runner, Duration::from_secs(5)).unwrap(), Vec::new());
        assert_eq!(runner.calls().len(), 1);
    }

    /// This Mac's real keychain parses (whatever it holds).
    #[test]
    fn this_macs_identities_parse() {
        if !std::path::Path::new("/usr/bin/security").exists() {
            return;
        }
        for team in teams(&SystemRunner, Duration::from_secs(20)).unwrap() {
            assert!(valid_team_id(&team.id) && !team.name.is_empty());
        }
    }
}
