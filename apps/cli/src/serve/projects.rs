//! The headless [`ProjectProvider`]: the roots `--project` named plus a
//! `projects.toml` beside the database — a server's project list is
//! configuration, not discovery.

use std::path::PathBuf;

use oximux_remote_host::ProjectProvider;
use oximux_remote_proto::messages::ProjectSummaryWire;

pub struct StaticProjects {
    roots: Vec<PathBuf>,
}

impl StaticProjects {
    /// Merge `--project` flags with `projects.toml` (`projects = ["/a", …]`)
    /// under the data dir. Non-directories are dropped with a log line rather
    /// than served — a client handed a path that does not exist would only
    /// discover it at `CreateSession`, which is a worse place to learn it.
    pub fn load(flags: Vec<PathBuf>, data_dir: &std::path::Path) -> Self {
        let mut roots = flags;
        let config = data_dir.join("projects.toml");
        if let Ok(raw) = std::fs::read_to_string(&config) {
            match toml::from_str::<ProjectsFile>(&raw) {
                Ok(parsed) => roots.extend(parsed.projects.into_iter().map(PathBuf::from)),
                Err(err) => {
                    tracing::warn!(%err, path = %config.display(), "unreadable projects.toml")
                }
            }
        }
        let mut seen = std::collections::HashSet::new();
        let roots = roots
            .into_iter()
            .filter_map(|root| match root.canonicalize() {
                Ok(canonical) if canonical.is_dir() => Some(canonical),
                _ => {
                    tracing::warn!(path = %root.display(), "project root is not a directory; skipped");
                    None
                }
            })
            .filter(|root| seen.insert(root.clone()))
            .collect();
        Self { roots }
    }
}

#[derive(serde::Deserialize)]
struct ProjectsFile {
    #[serde(default)]
    projects: Vec<String>,
}

#[async_trait::async_trait]
impl ProjectProvider for StaticProjects {
    async fn projects(&self) -> Vec<ProjectSummaryWire> {
        self.roots
            .iter()
            .map(|root| ProjectSummaryWire {
                name: root
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.to_string_lossy().into_owned()),
                path: root.to_string_lossy().into_owned(),
            })
            .collect()
    }
}
