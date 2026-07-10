use std::collections::HashMap;
use std::path::PathBuf;

use crate::cli::workspace::WorkspaceConfig;

/// Identifies a view in the navigation stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ViewId {
    /// Rooms-first main screen — the default landing view.
    MainMenu,
    /// Sub-menu for Agents, Policies, Orchestrators, Config.
    SettingsMenu,
    Policies,
    Agents,
    Rooms,
    Orchestrators,
    Settings,
    /// List of stored threads (the "Threads" tab landing view).
    Threads,
    /// One email-style thread. `id` selects a stored thread; `None` opens a
    /// fresh one.
    Thread {
        id: Option<String>,
    },
    JobDetail {
        job_id: String,
        orchestrator: String,
    },
}

/// Central application state for the TUI.
pub struct App {
    pub config: WorkspaceConfig,
    pub config_path: PathBuf,
    pub view_stack: Vec<ViewId>,
    pub should_quit: bool,
    pub status_message: Option<(String, super::views::StatusLevel)>,
    pub active_tab: usize,
    /// Thread id of a launch awaiting its `JobSubmitted` (which carries the
    /// job id). Promoted into [`Self::job_thread`] once the job id is known.
    /// A launch failure (no `JobSubmitted`) clears it.
    pub pending_thread_launch: Option<String>,
    /// Maps an in-flight deliberation's `job_id` → the thread that launched it,
    /// so a completion is attributed to the *correct* thread (never to whichever
    /// job merely finishes first). Entry removed when the job reaches a terminal
    /// state.
    pub job_thread: HashMap<String, String>,
    /// The policy currently acting as the "model" for new threads — shown in
    /// the footer and applied when composing. Defaults from the workspace.
    pub active_model: Option<String>,
    /// Effort (convergence threshold) override shown in the footer. `None` uses
    /// the policy's own default.
    pub active_effort: Option<f32>,
}

impl App {
    /// Create a new App from a workspace config file.
    pub fn new(config: WorkspaceConfig, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            // Land in the inbox (threads), the email-client front door.
            view_stack: vec![ViewId::Threads],
            should_quit: false,
            status_message: None,
            active_tab: 0,
            pending_thread_launch: None,
            job_thread: HashMap::new(),
            active_model: None,
            active_effort: None,
        }
    }

    /// Push a view onto the navigation stack.
    pub fn push_view(&mut self, view_id: ViewId) {
        self.view_stack.push(view_id);
    }

    /// Pop the current view. Returns `false` if at root (signals quit).
    pub fn pop_view(&mut self) -> bool {
        if self.view_stack.len() <= 1 {
            return false;
        }
        self.view_stack.pop();
        true
    }

    /// Get the currently active view.
    pub fn current_view(&self) -> Option<&ViewId> {
        self.view_stack.last()
    }

    /// Reload config from disk.
    pub fn reload_config(&mut self) -> Result<(), String> {
        match WorkspaceConfig::load_or_remote_default(&self.config_path) {
            Ok(config) => {
                self.config = config;
                Ok(())
            }
            Err(e) => Err(format!("Failed to reload config: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn minimal_config() -> WorkspaceConfig {
        WorkspaceConfig {
            policies: Default::default(),
            orchestrators: Default::default(),
            rooms: Default::default(),
            shared: None,
            default_room: None,
            agents: None,
        }
    }

    #[test]
    fn new_starts_at_inbox() {
        let app = App::new(minimal_config(), PathBuf::from("nsed.yaml"));
        assert_eq!(app.current_view(), Some(&ViewId::Threads));
        assert!(!app.should_quit);
        assert!(app.status_message.is_none());
    }

    #[test]
    fn push_and_pop_navigation() {
        let mut app = App::new(minimal_config(), PathBuf::from("nsed.yaml"));
        app.push_view(ViewId::Policies);
        app.push_view(ViewId::Agents);

        assert_eq!(app.current_view(), Some(&ViewId::Agents));
        assert_eq!(app.view_stack.len(), 3);

        assert!(app.pop_view()); // Agents → Policies
        assert_eq!(app.current_view(), Some(&ViewId::Policies));

        assert!(app.pop_view()); // Policies → Threads (root)
        assert_eq!(app.current_view(), Some(&ViewId::Threads));
    }

    #[test]
    fn pop_at_root_returns_false() {
        let mut app = App::new(minimal_config(), PathBuf::from("nsed.yaml"));
        assert!(!app.pop_view());
        // Stack unchanged
        assert_eq!(app.current_view(), Some(&ViewId::Threads));
    }

    #[test]
    fn reload_config_success() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nsed.yaml");
        let mut f = std::fs::File::create(&path).unwrap();
        // Valid config: room + policy + orchestrator all present
        writeln!(
            f,
            "policies:\n  review:\n    agents: [A, B]\n    max_rounds: 2\n    effort: 0.85\norchestrators:\n  local:\n    config_file: cfg.yml\nrooms:\n  test_room:\n    policy: review\n    orchestrator: local\ndefault_room: test_room"
        )
        .unwrap();

        let initial = WorkspaceConfig {
            policies: Default::default(),
            orchestrators: Default::default(),
            rooms: Default::default(),
            shared: None,
            default_room: None,
            agents: None,
        };
        let mut app = App::new(initial, path);
        assert!(app.config.default_room.is_none());

        app.reload_config().unwrap();
        assert_eq!(app.config.default_room.as_deref(), Some("test_room"));
    }

    #[test]
    #[serial_test::serial(home_env)]
    fn reload_config_missing_file() {
        // A missing config falls back to remote_workspace(), which reads
        // ~/.nsed. Point $HOME at an empty dir (and clear the endpoint env) so
        // the fallback finds nothing and reload errors — independent of the dev
        // machine's real ~/.nsed (which `quorum redeem` may have populated).
        let home = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        let prev_env = std::env::var_os("QUORUM_ORCHESTRATOR");
        // SAFETY: serialised via serial_test on the `home_env` group, so no
        // other test mutates these vars during this window.
        unsafe {
            std::env::set_var("HOME", home.path());
            std::env::remove_var("QUORUM_ORCHESTRATOR");
        }

        let mut app = App::new(minimal_config(), PathBuf::from("/nonexistent/nsed.yaml"));
        let result = app.reload_config();

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            if let Some(v) = prev_env {
                std::env::set_var("QUORUM_ORCHESTRATOR", v);
            }
        }

        assert!(
            result.is_err(),
            "missing config + empty ~/.nsed must error, got: {result:?}"
        );
    }

    #[test]
    fn view_id_equality() {
        assert_eq!(ViewId::MainMenu, ViewId::MainMenu);
        assert_ne!(ViewId::MainMenu, ViewId::Policies);
        assert_eq!(
            ViewId::JobDetail {
                job_id: "j1".into(),
                orchestrator: "o1".into()
            },
            ViewId::JobDetail {
                job_id: "j1".into(),
                orchestrator: "o1".into()
            }
        );
        assert_ne!(
            ViewId::JobDetail {
                job_id: "j1".into(),
                orchestrator: "o1".into()
            },
            ViewId::JobDetail {
                job_id: "j2".into(),
                orchestrator: "o1".into()
            }
        );
    }
}
