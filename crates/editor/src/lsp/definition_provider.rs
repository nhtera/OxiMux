//! Bridge `gpui-component`'s `DefinitionProvider` trait to `LspClient`.
//!
//! Returns `textDocument/definition` results as `Vec<LocationLink>`. The
//! widget itself acts on the links: a same-file target moves the cursor, and
//! Cmd-click highlights the symbol. (Opening a *different* file as a new
//! editor tab isn't expressible through this trait — the widget's internal
//! navigation only moves within the current buffer or opens http(s) URLs — so
//! cross-file jump-to-tab is handled separately by the host, not here.)

use std::sync::Arc;

use anyhow::Result;
use gpui::{App, Task, Window};
use gpui_component::input::{DefinitionProvider, Rope, RopeExt as _};
use lsp_types::{GotoDefinitionResponse, Location, LocationLink, Uri};
use tokio::runtime::Handle;

use super::client::LspClient;

/// DefinitionProvider for a single open buffer.
pub struct LspDefinitionProvider {
    client: Arc<LspClient>,
    uri: Uri,
    tokio_handle: Handle,
}

impl LspDefinitionProvider {
    pub fn new(client: Arc<LspClient>, uri: Uri) -> Self {
        let tokio_handle = client.tokio_handle();
        Self {
            client,
            uri,
            tokio_handle,
        }
    }
}

impl DefinitionProvider for LspDefinitionProvider {
    fn definitions(
        &self,
        text: &Rope,
        offset: usize,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<Result<Vec<LocationLink>>> {
        let position = text.offset_to_position(offset);
        let uri = self.uri.clone();
        let self_uri = self.uri.clone();
        let client = self.client.clone();
        let handle = self.tokio_handle.clone();
        cx.background_executor().spawn(async move {
            let resp = handle
                .spawn(async move { client.goto_definition(uri, position).await })
                .await
                .map_err(|join_err| anyhow::anyhow!("definition task join: {join_err}"))??;
            Ok(same_file_links(resp, &self_uri))
        })
    }
}

/// Convert a definition response to links and keep only same-file targets.
///
/// The widget navigates a `file://` target by moving the cursor *within the
/// current buffer*, so a cross-file target would jump to a meaningless offset.
/// Until host-driven open-in-new-tab lands, dropping cross-file links is a
/// clean no-op rather than a wrong jump. URI equality is exact-string (the
/// `lsp_types::Uri` `PartialEq`); both sides are `file://` URIs that the
/// server and `path_to_file_uri` canonicalize, so they match for the common
/// case. A symlinked workspace root with a server that returns non-canonical
/// URIs is the one case this would over-drop — acceptable vs. a wrong jump.
pub(crate) fn same_file_links(
    resp: Option<GotoDefinitionResponse>,
    self_uri: &Uri,
) -> Vec<LocationLink> {
    goto_response_to_links(resp)
        .into_iter()
        .filter(|link| &link.target_uri == self_uri)
        .collect()
}

/// Normalize the three `textDocument/definition` response shapes into the
/// `LocationLink` form the widget consumes.
pub(crate) fn goto_response_to_links(resp: Option<GotoDefinitionResponse>) -> Vec<LocationLink> {
    match resp {
        None => Vec::new(),
        Some(GotoDefinitionResponse::Scalar(loc)) => vec![location_to_link(loc)],
        Some(GotoDefinitionResponse::Array(locs)) => {
            locs.into_iter().map(location_to_link).collect()
        }
        Some(GotoDefinitionResponse::Link(links)) => links,
    }
}

/// A plain `Location` carries no origin range; promote it to a `LocationLink`
/// with the target range mirrored into the selection range.
fn location_to_link(loc: Location) -> LocationLink {
    LocationLink {
        origin_selection_range: None,
        target_uri: loc.uri,
        target_range: loc.range,
        target_selection_range: loc.range,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{Position, Range};
    use std::str::FromStr;

    fn uri(s: &str) -> Uri {
        Uri::from_str(s).unwrap()
    }

    fn range() -> Range {
        Range {
            start: Position::new(0, 0),
            end: Position::new(0, 1),
        }
    }

    fn loc(u: &str) -> Location {
        Location {
            uri: uri(u),
            range: range(),
        }
    }

    #[test]
    fn none_response_yields_no_links() {
        assert!(goto_response_to_links(None).is_empty());
    }

    #[test]
    fn scalar_and_array_locations_become_links() {
        let scalar = goto_response_to_links(Some(GotoDefinitionResponse::Scalar(loc(
            "file:///a.rs",
        ))));
        assert_eq!(scalar.len(), 1);
        assert_eq!(scalar[0].target_uri, uri("file:///a.rs"));

        let array = goto_response_to_links(Some(GotoDefinitionResponse::Array(vec![
            loc("file:///a.rs"),
            loc("file:///b.rs"),
        ])));
        assert_eq!(array.len(), 2);
    }

    #[test]
    fn same_file_filter_keeps_only_current_uri() {
        let here = uri("file:///a.rs");
        let resp = Some(GotoDefinitionResponse::Array(vec![
            loc("file:///a.rs"), // same file → kept
            loc("file:///b.rs"), // cross file → dropped
        ]));
        let links = same_file_links(resp, &here);
        assert_eq!(links.len(), 1, "only the same-file target survives");
        assert_eq!(links[0].target_uri, here);
    }

    #[test]
    fn same_file_filter_drops_all_cross_file() {
        let here = uri("file:///a.rs");
        let resp = Some(GotoDefinitionResponse::Scalar(loc("file:///elsewhere.rs")));
        assert!(
            same_file_links(resp, &here).is_empty(),
            "a lone cross-file definition is a clean no-op"
        );
    }
}
