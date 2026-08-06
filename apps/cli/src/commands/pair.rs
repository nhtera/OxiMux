//! `oximux pair-new` / `pair-ls` / `pair-rm` — pairing administration against
//! the running host, over the owner-only local socket. The host enforces the
//! operator-only gate; this side enforces the OTHER half of the ticket's
//! safety story: a bearer credential prints to an interactive terminal only.

use std::io::IsTerminal as _;

use oximux_remote_proto::proto::{Request, Response};
use serde_json::{Value, json};

use crate::cli::exit;
use crate::client::{Client, rpc_failure, unexpected_reply};
use crate::output::Failure;

/// Render the ticket as a scannable terminal QR (the same payload the desktop
/// shows). Best-effort — a ticket too long to encode still prints as text.
fn ticket_qr(ticket: &str) -> Option<String> {
    let code = qrcode::QrCode::new(ticket.as_bytes()).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    )
}

pub async fn pair_new(
    client: &Client,
    read_only: bool,
    force_non_tty: bool,
    json_mode: bool,
) -> Result<(Value, String), Failure> {
    // The TTY guard is load-bearing, not cosmetic: with write-by-default
    // enrollments, TTY-only printing + one-time + short expiry are the three
    // properties standing between a captured ticket and a write-capable
    // device. Refuse BEFORE asking the host to open a window.
    if !std::io::stdout().is_terminal() && !force_non_tty {
        return Err(Failure::new(
            "not-a-tty",
            exit::USAGE,
            "refusing to print a pairing ticket to a non-terminal (it is a bearer credential)",
        )
        .with_steps([
            "run this in an interactive terminal, or".into(),
            "pass --force-non-tty if you accept responsibility for where the ticket lands".into(),
        ]));
    }
    let issued = match client.call(Request::PairNew { read_only }).await? {
        Response::PairingIssued(issued) => issued,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("PairNew", &other)),
    };
    let tier = if issued.read_only { "read-only" } else { "full write" };
    let mut human = String::new();
    if let Some(qr) = ticket_qr(&issued.ticket) {
        human.push_str(&qr);
        human.push('\n');
    }
    human.push_str(&format!(
        "pairing ticket ({tier}, one-time, expires in ~2 minutes):\n{}\n\
         scan the QR with the mobile app, or paste the ticket into a client.\n\
         review enrollments with `oximux pair-ls`.",
        issued.ticket
    ));
    let data = json!({
        "ticket": issued.ticket,
        "expires_at": issued.expires_at,
        "read_only": issued.read_only,
    });
    // In JSON mode the envelope IS the output; the caller asked for machine
    // form and passed the TTY guard, so the ticket rides in `data`.
    let _ = json_mode;
    Ok((data, human))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub async fn pair_ls(client: &Client) -> Result<(Value, String), Failure> {
    let rows = match client.call(Request::PairList).await? {
        Response::PairedDeviceList(rows) => rows,
        Response::Error(e) => return Err(rpc_failure(e)),
        other => return Err(unexpected_reply("PairList", &other)),
    };
    let human = if rows.is_empty() {
        "no paired devices".to_string()
    } else {
        rows.iter()
            .map(|d| {
                let tier = if d.read_only { "read-only " } else { "full-write" };
                let revoked = if d.revoked { "  [revoked]" } else { "" };
                let seen = d
                    .last_seen
                    .map(|s| format!("last seen {s}"))
                    .unwrap_or_else(|| "never reconnected".into());
                format!("{}  {tier}  {}  ({seen}){revoked}", hex(&d.pubkey), d.name)
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let data = json!(rows
        .iter()
        .map(|d| json!({
            "pubkey": hex(&d.pubkey),
            "name": d.name,
            "read_only": d.read_only,
            "revoked": d.revoked,
            "last_seen": d.last_seen,
        }))
        .collect::<Vec<_>>());
    Ok((data, human))
}

pub async fn pair_rm(client: &Client, pubkey_hex: &str) -> Result<(Value, String), Failure> {
    let cleaned = pubkey_hex.trim();
    let bytes = (cleaned.len() == 64)
        .then(|| {
            (0..32)
                .map(|i| u8::from_str_radix(&cleaned[i * 2..i * 2 + 2], 16).ok())
                .collect::<Option<Vec<u8>>>()
        })
        .flatten()
        .ok_or_else(|| {
            Failure::new(
                "bad-pubkey",
                exit::USAGE,
                "the pubkey must be 64 hex characters, as `pair-ls` prints it",
            )
        })?;
    let mut pubkey = [0u8; 32];
    pubkey.copy_from_slice(&bytes);
    match client.call(Request::PairRemove { pubkey }).await? {
        Response::Ack => Ok((
            json!({ "removed": cleaned }),
            format!("removed {cleaned} — that device may pair again with a fresh ticket"),
        )),
        Response::Error(e) => Err(rpc_failure(e)),
        other => Err(unexpected_reply("PairRemove", &other)),
    }
}
