//! Fetching release bytes, and the seam that lets the pipeline be tested
//! without a network.
//!
//! Everything the updater reads goes through [`Fetcher`], so the ordering the
//! pipeline depends on — signature before parse, digest before extract, gate
//! before swap — is exercised in-process against hand-made tampering rather
//! than against a live release.

use std::io::Read as _;
use std::time::Duration;

use super::ReleaseError;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(60);
/// GitHub rejects requests without one.
const USER_AGENT: &str = "oximux-updater";

/// Where release assets live. `latest/download/…` is GitHub's redirect to the
/// most recent **published** release, which is why a draft release — what the
/// workflow creates before notes are curated — is invisible to the updater
/// until someone publishes it.
const RELEASE_LATEST: &str = "https://github.com/nhtera/OxiMux/releases/latest/download";

pub trait Fetcher {
    /// Read at most `ceiling` bytes from `url`, or fail. A response larger
    /// than the ceiling is an error, never a truncation.
    fn get(&self, url: &str, ceiling: u64) -> Result<Vec<u8>, ReleaseError>;
}

pub struct HttpFetcher;

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str, ceiling: u64) -> Result<Vec<u8>, ReleaseError> {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout_read(READ_TIMEOUT)
            .user_agent(USER_AGENT)
            .build();
        let response = agent.get(url).call().map_err(transport)?;

        // The *final* URL after redirects is what actually served the bytes.
        // Signature verification is downstream and authoritative, but a
        // redirect chain that left GitHub means something is wrong well
        // before that, and stopping here costs one comparison.
        let final_host = url::Url::parse(response.get_url())
            .ok()
            .and_then(|u| u.host_str().map(|host| host.to_string()))
            .unwrap_or_default();
        if !host_allowed(&final_host) {
            return Err(ReleaseError::DisallowedHost { host: final_host });
        }

        let mut body = Vec::new();
        response
            .into_reader()
            .take(ceiling.saturating_add(1))
            .read_to_end(&mut body)
            .map_err(|err| ReleaseError::Network { detail: format!("download interrupted: {err}") })?;
        if body.len() as u64 > ceiling {
            return Err(ReleaseError::Oversize { ceiling });
        }
        Ok(body)
    }
}

fn transport(err: ureq::Error) -> ReleaseError {
    match err {
        ureq::Error::Status(404, _) => ReleaseError::NoRelease,
        ureq::Error::Status(403, _) | ureq::Error::Status(429, _) => ReleaseError::RateLimited,
        ureq::Error::Status(code, _) => {
            ReleaseError::Network { detail: format!("the release server returned HTTP {code}") }
        }
        ureq::Error::Transport(t) => ReleaseError::Network { detail: t.to_string() },
    }
}

/// GitHub release assets redirect to `*.githubusercontent.com`; anything else
/// means the chain left GitHub.
fn host_allowed(host: &str) -> bool {
    if host == "github.com" || host == "api.github.com" {
        return true;
    }
    if host.ends_with(".githubusercontent.com") {
        return true;
    }
    #[cfg(debug_assertions)]
    if host == "localhost" || host == "127.0.0.1" {
        return true;
    }
    false
}

/// The base every URL is built from. Overridable only in debug builds, and
/// only so an end-to-end test can serve a fake release — a release binary
/// contains no way to repoint its own trust chain.
fn latest_base() -> String {
    #[cfg(debug_assertions)]
    if let Ok(base) = std::env::var("OXIMUX_UPDATE_BASE_URL") {
        return base.trim_end_matches('/').to_string();
    }
    RELEASE_LATEST.to_string()
}

pub fn manifest_url() -> String {
    format!("{}/manifest.json", latest_base())
}

pub fn signature_url() -> String {
    format!("{}/manifest.json.minisig", latest_base())
}

/// An asset's URL, built from the *signed* tag and file name rather than read
/// out of the manifest. A manifest therefore cannot name a download host of
/// its own choosing, and [`host_allowed`] stays meaningful.
pub fn asset_url(tag: &str, name: &str) -> String {
    let base = latest_base();
    // `…/releases/latest/download` → `…/releases/download/<tag>`, the
    // version-pinned form. A debug override keeps its own shape so a test
    // server can serve flat paths.
    match base.strip_suffix("/latest/download") {
        Some(root) => format!("{root}/download/{tag}/{name}"),
        None => format!("{base}/{name}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_github_hosts_pass_the_allow_list() {
        assert!(host_allowed("github.com"));
        assert!(host_allowed("objects.githubusercontent.com"));
        assert!(host_allowed("release-assets.githubusercontent.com"));
        for bad in ["github.com.evil.example", "githubusercontent.com.evil.example", "", "evil.io"]
        {
            assert!(!host_allowed(bad), "{bad} must not pass");
        }
    }

    /// The asset URL is pinned to the tag the signed manifest declares, so a
    /// tampered manifest cannot redirect the download elsewhere.
    #[test]
    fn asset_urls_are_pinned_to_the_signed_tag() {
        // Guard against a stray override from the surrounding environment.
        if std::env::var_os("OXIMUX_UPDATE_BASE_URL").is_some() {
            return;
        }
        let url = asset_url("v0.2.0", "oximux-0.2.0-aarch64-apple-darwin.tar.gz");
        assert_eq!(
            url,
            "https://github.com/nhtera/OxiMux/releases/download/v0.2.0/\
             oximux-0.2.0-aarch64-apple-darwin.tar.gz"
        );
        assert!(manifest_url().ends_with("/latest/download/manifest.json"));
        assert_eq!(signature_url(), format!("{}.minisig", manifest_url()));
    }
}
