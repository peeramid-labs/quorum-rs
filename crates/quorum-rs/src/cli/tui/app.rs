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
    Orchestrators,
    Settings,
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
}

impl App {
    /// Create a new App from a workspace config file.
    pub fn new(config: WorkspaceConfig, config_path: PathBuf) -> Self {
        Self {
            config,
            config_path,
            view_stack: vec![ViewId::MainMenu],
            should_quit: false,
            status_message: None,
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
        match WorkspaceConfig::load(&self.config_path) {
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
    fn new_starts_at_main_menu() {
        let app = App::new(minimal_config(), PathBuf::from("nsed.yaml"));
        assert_eq!(app.current_view(), Some(&ViewId::MainMenu));
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

        assert!(app.pop_view()); // Policies → MainMenu
        assert_eq!(app.current_view(), Some(&ViewId::MainMenu));
    }

    #[test]
    fn pop_at_root_returns_false() {
        let mut app = App::new(minimal_config(), PathBuf::from("nsed.yaml"));
        assert!(!app.pop_view());
        // Stack unchanged
        assert_eq!(app.current_view(), Some(&ViewId::MainMenu));
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
    fn reload_config_missing_file() {
        let mut app = App::new(minimal_config(), PathBuf::from("/nonexistent/nsed.yaml"));
        let result = app.reload_config();
        assert!(result.is_err());
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
