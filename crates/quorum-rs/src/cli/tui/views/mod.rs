pub mod agents;
pub mod common;
pub mod job_detail;
pub mod main_menu;
pub mod orchestrators;
pub mod policies;
pub mod rooms;
pub mod settings;
pub mod settings_menu;

use ratatui::Frame;
use ratatui::layout::Rect;

use super::app::ViewId;
use super::event::AppEvent;

/// Actions a view can request from the main loop.
///
/// Views never touch the terminal or async runtime directly — they return
/// actions that the dispatcher interprets.
#[derive(Debug, Clone, PartialEq)]
pub enum ViewAction {
    /// Push a new view onto the navigation stack.
    Push(ViewId),
    /// Pop the current view (go back).
    Pop,
    /// Quit the application.
    Quit,
    /// Fetch data from a remote orchestrator.
    Fetch(FetchRequest),
    /// Launch a deliberation job.
    LaunchJob {
        orchestrator: String,
        task: String,
        room: Option<String>,
        policy: Option<String>,
        /// Optional override for the room/policy's `effort` (convergence
        /// threshold, in `[0.0, 1.0]`). `None` uses the policy's default.
        /// Set from the main-menu launcher when the operator types a
        /// custom threshold before submitting.
        effort_override: Option<f32>,
    },
    /// Apply a config mutation to nsed.yaml.
    WriteConfig(ConfigMutation),
    /// Inject a message into a running deliberation.
    InjectMessage {
        orchestrator: String,
        job_id: String,
        message: String,
    },
    /// Show a transient status message.
    SetStatus(String, StatusLevel),
}

/// Async data fetch requests dispatched to the `TuiClient`.
#[derive(Debug, Clone, PartialEq)]
pub enum FetchRequest {
    Policies {
        orchestrator: String,
        tag: Option<String>,
    },
    Agents {
        orchestrator: String,
    },
    Rooms {
        orchestrator: String,
    },
    /// Create (or replace) a room via `POST /admin/api/rooms`.
    CreateRoom {
        orchestrator: String,
        id: String,
        tags: Vec<String>,
        visibility: String,
    },
    /// Delete a room via `DELETE /admin/api/rooms/{id}`.
    DeleteRoom {
        orchestrator: String,
        id: String,
    },
    Health {
        orchestrator: String,
    },
    StartSseStream {
        orchestrator: String,
        job_id: String,
    },
}

/// Config file mutations applied atomically via `config_writer`.
#[derive(Debug, Clone, PartialEq)]
pub enum ConfigMutation {
    AddRoom {
        name: String,
        policy: String,
        orchestrator: String,
    },
    EditRoom {
        name: String,
        policy: Option<String>,
        orchestrator: Option<String>,
    },
    SetDefaultRoom(String),
}

/// Status message severity level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatusLevel {
    Info,
    Success,
    Error,
}

/// Trait for all TUI views.
///
/// Views are pure state machines — `update` returns actions, `draw` renders.
/// They never call async functions or modify the terminal directly.
pub trait View {
    /// Process an event and optionally return an action.
    fn update(&mut self, event: &AppEvent) -> Option<ViewAction>;

    /// Render the view into the given frame area.
    fn draw(&mut self, frame: &mut Frame, area: Rect);

    /// Called when the view becomes the active (top-of-stack) view.
    /// Returns actions to execute (e.g., initial data fetches).
    fn on_enter(&mut self) -> Vec<ViewAction> {
        Vec::new()
    }
}
