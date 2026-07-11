pub mod agents;
pub mod common;
pub mod job_detail;
pub mod main_menu;
pub mod orchestrators;
pub mod policies;
pub mod rooms;
pub mod settings;
pub mod settings_menu;
pub mod thread;
pub mod thread_list;

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
        /// Id of the thread this launch belongs to, when submitted from a
        /// thread view. The loop records the deliberation's reply back into
        /// this thread on completion. `None` for one-shot launches.
        thread_id: Option<String>,
        /// Per-branch session key (the replied-to branch's `branch_id`). Keys
        /// the claude session so a linear branch resumes and a fork gets a fresh
        /// session. `None` falls back to the room id (one-shot / non-thread).
        conversation_id: Option<String>,
        /// The new turn only (this send's message). Sent so a resumed session's
        /// delta prompt carries just this, not the whole flattened `task`. `None`
        /// for the first turn (fresh — needs the full task).
        new_turn: Option<String>,
        /// The conversation as a role-tagged message array — the native form the
        /// agent renders per session-resume state (supersedes task + new_turn).
        messages: Vec<crate::conversation::Message>,
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
    /// Open the deliberation detail for a thread's in-flight job (user-driven,
    /// via `Ctrl-D` in the thread view). The loop resolves the thread's active
    /// `job_id` and pushes the detail view, or reports none is running.
    OpenThreadJob {
        thread_id: String,
        orchestrator: String,
    },
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
        policy: Option<String>,
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
    /// Reconcile a reopened thread's pending job — fetch its result and append
    /// the reply if the deliberation has finished (recovers replies that landed
    /// while the TUI was closed).
    ReconcileThread {
        orchestrator: String,
        job_id: String,
        thread_id: String,
    },
    /// Cancel (kill) a thread's running deliberation (Ctrl-C). Aborts it with no
    /// result and clears the thread's pending marker so a follow-up can continue.
    CancelJob {
        orchestrator: String,
        job_id: String,
        thread_id: String,
    },
    /// Answer an agent's pending `ask_user` question — POST the result so the
    /// blocked agent resumes.
    RespondToolCall {
        orchestrator: String,
        job_id: String,
        call_id: String,
        result: String,
    },
    /// Fetch a job's pending `ask_user` questions (GET). Recovers a question that
    /// fired while the thread view wasn't focused — surfaced on reopen.
    PendingToolCalls {
        orchestrator: String,
        job_id: String,
        thread_id: String,
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

    /// `true` when the view is capturing free-text/keystroke input (a form
    /// or filter field), so the shell must NOT steal number/Tab keys for
    /// top-level tab switching. Defaults to `false`.
    fn captures_input(&self) -> bool {
        false
    }

    /// The model (policy) this view is currently acting under, shown in the
    /// footer. `None` lets the footer fall back to the app-wide default.
    fn active_model(&self) -> Option<&str> {
        None
    }

    /// The effort (convergence threshold) this view is acting under, shown in
    /// the footer. `None` lets the footer fall back to the app-wide default.
    fn active_effort(&self) -> Option<f32> {
        None
    }
}
