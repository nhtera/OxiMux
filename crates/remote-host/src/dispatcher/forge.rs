//! The read-only forge RPCs: issues, pull requests, and CI checks for the
//! repository a session lives in.
//!
//! **No credential crosses this boundary.** Every query shells out to the
//! `gh`/`glab` CLI already signed in on the desktop, so the phone holds no
//! token, runs no OAuth flow, and stores nothing. That is not a convenience —
//! it is the reason this surface can exist at all without a credential-handling
//! story on a device that is easier to lose than a laptop.
//!
//! The repository is resolved from the session's own `cwd`, the same way the git
//! RPCs do it, so forge access inherits the device's existing session ACL: a
//! session-scoped device cannot enumerate another project's issues.
//!
//! **Empty is a normal answer here, and this module must keep it that way.** The
//! underlying transport degrades to empty for every "can't tell" case — CLI
//! absent, CLI signed out, repo not forge-hosted, network down — and the
//! temptation is to translate some of those into errors so the client can say
//! something more specific. It cannot: the host genuinely does not know which
//! case it hit, and inventing a reason would put a wrong explanation in front of
//! the user. So the contract is forwarded unchanged and the client renders
//! "nothing here".

use std::path::PathBuf;

use oximux_remote_proto::messages::{
    CheckRunWire, ForgeItemDetailWire, ForgeItemKindWire, ForgeItemWire, ForgeStateWire,
};
use oximux_remote_proto::proto::{Response, RpcError};

use super::Dispatcher;
use crate::auth::Peer;

impl Dispatcher {
    /// The session's working directory behind the read gate.
    ///
    /// Deliberately **not** reusing the git handlers' `session_repo`: that opens
    /// a git repository, and a forge query does not need one. A directory that
    /// is not a git repo at all still has a defensible answer here (no items),
    /// and routing through `Repository::open` would turn that into an error.
    /// Returns the bare [`RpcError`] rather than a whole `Response`: every
    /// failure here is one, and `Response` is a much larger type to move on an
    /// error path the callers immediately re-wrap anyway.
    fn session_cwd(&self, peer: &Peer, session_id: &str) -> Result<PathBuf, RpcError> {
        // A read, so `is_allowed_for` rather than `may_write` — nothing on this
        // surface mutates. PR create and merge are deliberately not here; a
        // merge is effectively irreversible and wants its own decision.
        if !self.auth.is_allowed_for(peer, session_id) {
            return Err(RpcError::Unauthorized);
        }
        let Some(handle) = self.registry.get(session_id) else {
            return Err(RpcError::UnknownSession);
        };
        handle
            .meta_snapshot()
            .cwd
            .ok_or_else(|| RpcError::BadRequest("session has no working directory".into()))
    }

    /// Issues or pull requests for the session's repository.
    pub(super) async fn list_forge_items(
        &self,
        peer: &Peer,
        session_id: &str,
        kind: ForgeItemKindWire,
        state: ForgeStateWire,
        mine: bool,
    ) -> Response {
        let cwd = match self.session_cwd(peer, session_id) {
            Ok(cwd) => cwd,
            Err(e) => return Response::Error(e),
        };
        let filter = oximux_git::gh::ForgeListFilter {
            state: to_state(state),
            mine,
            // No free-text search from the phone. The value is a raw forge-search
            // query that would be forwarded into a CLI argument, and there is no
            // phone-side affordance that needs it — so it is simply not offered
            // rather than plumbed and left unused.
            search: None,
        };
        let items = oximux_git::forge::list_items(&cwd, to_kind(kind), filter).await;
        Response::ForgeItems(items.into_iter().map(to_item_wire).collect())
    }

    /// Body + author of one issue/PR.
    pub(super) async fn forge_item_detail(
        &self,
        peer: &Peer,
        session_id: &str,
        kind: ForgeItemKindWire,
        number: u64,
    ) -> Response {
        let cwd = match self.session_cwd(peer, session_id) {
            Ok(cwd) => cwd,
            Err(e) => return Response::Error(e),
        };
        let detail = oximux_git::forge::item_detail(&cwd, to_kind(kind), number).await;
        Response::ForgeItemDetail(detail.map(|d| ForgeItemDetailWire {
            body: d.body,
            author: d.author.login,
        }))
    }

    /// CI check runs for the current branch's pull request.
    pub(super) async fn list_forge_checks(
        &self,
        peer: &Peer,
        session_id: &str,
    ) -> Response {
        let cwd = match self.session_cwd(peer, session_id) {
            Ok(cwd) => cwd,
            Err(e) => return Response::Error(e),
        };
        let checks = oximux_git::forge::checks(&cwd).await;
        Response::ForgeChecks(
            checks
                .into_iter()
                .map(|c| CheckRunWire {
                    name: c.name,
                    bucket: c.bucket,
                    link: c.link,
                    description: c.description,
                })
                .collect(),
        )
    }
}

fn to_kind(kind: ForgeItemKindWire) -> oximux_core::ForgeRefKind {
    match kind {
        ForgeItemKindWire::Issue => oximux_core::ForgeRefKind::Issue,
        ForgeItemKindWire::Pull => oximux_core::ForgeRefKind::Pull,
    }
}

fn to_state(state: ForgeStateWire) -> oximux_git::gh::ForgeState {
    match state {
        ForgeStateWire::Open => oximux_git::gh::ForgeState::Open,
        ForgeStateWire::Closed => oximux_git::gh::ForgeState::Closed,
        ForgeStateWire::All => oximux_git::gh::ForgeState::All,
    }
}

/// Flatten a forge item onto the wire.
///
/// Labels and assignees collapse to plain strings: the source types carry a
/// single field each (`name`, `login`), and mirroring two more structs across
/// the wire to hold one string apiece would be shape for its own sake.
fn to_item_wire(item: oximux_git::gh::ForgeItem) -> ForgeItemWire {
    ForgeItemWire {
        number: item.number,
        title: item.title,
        state: item.state,
        url: item.url,
        labels: item.labels.into_iter().map(|l| l.name).collect(),
        assignees: item.assignees.into_iter().map(|a| a.login).collect(),
        author: item.author.login,
        updated_at: item.updated_at,
    }
}
