//! What OxiMux needs from the machine, and how to get it.
//!
//! Pure data and pure wording — no probing, no spawning. The catalog is
//! separate from [`super::probe`] so the two questions stay separate: *what
//! could be here* is a fixed list this file owns, and *what is actually here*
//! is a syscall someone else makes.
//!
//! Every entry names what **stops working** without it, not what it "enables".
//! A person reading "Not installed" next to "GitHub CLI" needs to know whether
//! that explains the thing they are currently confused about, and "adds GitHub
//! integration" does not answer that. "Pull requests, CI checks, and GitHub
//! issues in Tasks" does.

/// An external command-line tool OxiMux calls out to.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub(crate) enum Tool {
    Git,
    GithubCli,
    GitlabCli,
    Ripgrep,
}

/// A package-manager invocation that installs one tool.
///
/// Program and arguments rather than a shell string: the pane both *runs* this
/// and offers it for copying, and a single spelling is the only way those two
/// cannot drift. The arguments are the non-interactive ones on purpose —
/// whatever the button runs is exactly what the copied command runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Recipe {
    /// The package manager's binary, which must itself be on PATH before this
    /// recipe is offered.
    pub manager: &'static str,
    pub args: Vec<String>,
}

impl Recipe {
    /// The command as a person would type it — for the clipboard, and for the
    /// "run this yourself" case where the manager is absent.
    pub fn command_line(&self) -> String {
        std::iter::once(self.manager.to_string())
            .chain(self.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

impl Tool {
    /// Every tool the pane lists, in the order it lists them: the one the app
    /// cannot run without, then the two forge CLIs, then search.
    pub(crate) const ALL: [Tool; 4] = [
        Tool::Git,
        Tool::GithubCli,
        Tool::GitlabCli,
        Tool::Ripgrep,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Tool::Git => "Git",
            Tool::GithubCli => "GitHub CLI",
            Tool::GitlabCli => "GitLab CLI",
            Tool::Ripgrep => "ripgrep",
        }
    }

    /// The binary name, as typed and as looked up on PATH.
    pub(crate) fn binary(self) -> &'static str {
        match self {
            Tool::Git => "git",
            Tool::GithubCli => "gh",
            Tool::GitlabCli => "glab",
            Tool::Ripgrep => "rg",
        }
    }

    /// What stops working without it. Named surfaces, not capabilities.
    pub(crate) fn needed_for(self) -> &'static str {
        match self {
            Tool::Git => "Source control, diffs, branches, worktrees — everything git-shaped.",
            Tool::GithubCli => {
                "Pull requests, CI checks, and GitHub issues on the Tasks page."
            }
            Tool::GitlabCli => {
                "Merge requests, pipelines, and GitLab issues on the Tasks page."
            }
            Tool::Ripgrep => "The Search panel and Quick Open's file index.",
        }
    }

    /// Where to read about installing it by hand. Shown when there is no
    /// package manager to click, and always available as the escape hatch when
    /// an install fails for a reason the pane cannot explain.
    pub(crate) fn docs_url(self) -> &'static str {
        match self {
            Tool::Git => "https://git-scm.com/downloads",
            Tool::GithubCli => "https://cli.github.com/",
            Tool::GitlabCli => "https://gitlab.com/gitlab-org/cli",
            Tool::Ripgrep => "https://github.com/BurntSushi/ripgrep#installation",
        }
    }

    /// Whether being present is the whole story, or a sign-in follows.
    ///
    /// Only the forge CLIs have a second step, and conflating the two states
    /// would be the single most misleading thing this pane could do: an
    /// installed-but-signed-out `gh` looks exactly like a working one from the
    /// outside and fails every call.
    pub(crate) fn has_sign_in(self) -> bool {
        matches!(self, Tool::GithubCli | Tool::GitlabCli)
    }

    /// The command that signs the tool in, for the hint text. `None` when the
    /// tool has no sign-in at all.
    pub(crate) fn sign_in_command(self) -> Option<&'static str> {
        match self {
            Tool::GithubCli => Some("gh auth login"),
            Tool::GitlabCli => Some("glab auth login"),
            _ => None,
        }
    }

    /// Whether OxiMux is meaningfully broken without it, as opposed to
    /// missing one surface. Drives whether "Not installed" reads as a warning
    /// or as an error.
    pub(crate) fn is_required(self) -> bool {
        matches!(self, Tool::Git)
    }

    /// Whether the packaged app ships its own copy, so a PATH miss is not the
    /// whole answer. Only ripgrep is bundled (see [`crate::shell::tool_paths`]).
    pub(crate) fn is_bundled(self) -> bool {
        matches!(self, Tool::Ripgrep)
    }

    /// How to install it on this platform, or `None` where the pane has no
    /// command it is confident in.
    ///
    /// Linux deliberately returns `None` for everything. The distro spread is
    /// the reason: `gh` and `glab` are not in most default repositories and
    /// need a vendor repo added first, and shipping a command that fails —
    /// or worse, one that silently installs something else — is worse than
    /// showing the documentation link and getting out of the way. Windows and
    /// macOS each have one package manager that covers all four.
    pub(crate) fn install_recipe(self) -> Option<Recipe> {
        #[cfg(windows)]
        {
            let id = match self {
                Tool::Git => "Git.Git",
                Tool::GithubCli => "GitHub.cli",
                Tool::GitlabCli => "GLab.GLab",
                Tool::Ripgrep => "BurntSushi.ripgrep.MSVC",
            };
            let mut args = vec!["install".to_string(), "--id".to_string(), id.to_string()];
            args.extend(WINGET_FLAGS.iter().map(|f| f.to_string()));
            Some(Recipe {
                manager: "winget",
                args,
            })
        }
        #[cfg(target_os = "macos")]
        {
            let formula = match self {
                Tool::Git => "git",
                Tool::GithubCli => "gh",
                Tool::GitlabCli => "glab",
                Tool::Ripgrep => "ripgrep",
            };
            Some(Recipe {
                manager: "brew",
                args: vec!["install".to_string(), formula.to_string()],
            })
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        {
            let _ = self;
            None
        }
    }
}

// winget package ids are verified against `winget search`, not remembered.
//
// The flags are what make this runnable from a GUI process with no console:
// without the agreement flags winget stops on a prompt nobody can see, and
// `--disable-interactivity` turns any remaining prompt into a non-zero exit
// the pane can report instead of a hang. One list, so the command the button
// runs and the command the clipboard offers cannot drift apart.
#[cfg(windows)]
const WINGET_FLAGS: &[&str] = &[
    "--exact",
    "--accept-package-agreements",
    "--accept-source-agreements",
    "--disable-interactivity",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_described_and_documented() {
        for tool in Tool::ALL {
            assert!(!tool.name().is_empty());
            assert!(!tool.binary().is_empty());
            assert!(!tool.needed_for().is_empty(), "{:?}", tool);
            assert!(
                tool.docs_url().starts_with("https://"),
                "{:?} must link somewhere a person can actually read",
                tool
            );
        }
    }

    #[test]
    fn what_breaks_is_named_not_generalised() {
        // Regression guard on the wording rule this module exists to hold: a
        // person reading "Not installed" needs to know whether it explains the
        // thing confusing them right now.
        for tool in Tool::ALL {
            let text = tool.needed_for().to_lowercase();
            assert!(
                !text.contains("enables") && !text.contains("integration with"),
                "{:?} describes a capability instead of naming what stops working: {}",
                tool,
                tool.needed_for()
            );
        }
    }

    #[test]
    fn only_the_forge_clis_have_a_sign_in() {
        assert!(Tool::GithubCli.has_sign_in());
        assert!(Tool::GitlabCli.has_sign_in());
        assert!(!Tool::Git.has_sign_in());
        assert!(!Tool::Ripgrep.has_sign_in());
    }

    #[test]
    fn a_sign_in_command_exists_exactly_where_a_sign_in_does() {
        for tool in Tool::ALL {
            assert_eq!(
                tool.has_sign_in(),
                tool.sign_in_command().is_some(),
                "{:?} disagrees with itself about whether it can be signed in",
                tool
            );
        }
    }

    #[test]
    fn git_is_the_only_hard_requirement() {
        assert!(Tool::Git.is_required());
        for tool in Tool::ALL.into_iter().filter(|t| *t != Tool::Git) {
            assert!(!tool.is_required(), "{:?}", tool);
        }
    }

    #[test]
    fn a_recipe_renders_as_the_command_it_runs() {
        // The copied text must be the invocation, not a prettier paraphrase —
        // a user who pastes it and gets different behaviour has been lied to.
        let recipe = Recipe {
            manager: "brew",
            args: vec!["install".to_string(), "gh".to_string()],
        };
        assert_eq!(recipe.command_line(), "brew install gh");
    }

    #[cfg(windows)]
    #[test]
    fn every_windows_recipe_is_non_interactive_and_pinned_by_id() {
        // A GUI process has no console for winget to prompt into, so a recipe
        // missing these flags does not fail — it hangs. And `--id` matters:
        // installing by *name* can match a different package entirely.
        for tool in Tool::ALL {
            let recipe = tool.install_recipe().expect("windows has winget");
            assert_eq!(recipe.manager, "winget");
            for flag in WINGET_FLAGS {
                assert!(
                    recipe.args.iter().any(|a| a == flag),
                    "{:?} is missing {flag}",
                    tool
                );
            }
            assert!(
                recipe.args.iter().any(|a| a == "--id"),
                "{:?} installs by name, not id",
                tool
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn no_two_tools_share_a_package_id() {
        // A copy-paste in the id match would install one tool twice and report
        // the other as fixed.
        let mut ids: Vec<String> = Tool::ALL
            .into_iter()
            .map(|t| t.install_recipe().expect("winget").args[2].clone())
            .collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate winget id: {ids:?}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn every_macos_recipe_goes_through_brew() {
        for tool in Tool::ALL {
            let recipe = tool.install_recipe().expect("macos has brew");
            assert_eq!(recipe.manager, "brew");
            assert_eq!(recipe.args.first().map(String::as_str), Some("install"));
        }
    }

    #[cfg(not(any(windows, target_os = "macos")))]
    #[test]
    fn linux_offers_documentation_rather_than_a_guess() {
        for tool in Tool::ALL {
            assert!(
                tool.install_recipe().is_none(),
                "{:?}: a command that fails is worse than a link that works",
                tool
            );
        }
    }
}
