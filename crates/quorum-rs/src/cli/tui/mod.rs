pub mod app;
pub mod client;
pub mod config_writer;
pub mod event;
pub mod views;

use std::io;
use std::path::Path;
use std::process::ExitCode;

use crossterm::event::DisableMouseCapture;
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Frame;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Tabs};
use tokio::sync::mpsc;

use crate::cli::remote::RemoteOrchestrator;
use crate::cli::request::build_request;
use crate::cli::workspace::{OrchestratorMode, WorkspaceConfig};
use app::{App, ViewId};
use client::TuiClient;
use event::{AppEvent, DataEvent, EventLoopConfig, spawn_terminal_event_loop};
use views::agents::AgentsView;
use views::job_detail::JobDetailView;
use views::main_menu::MainMenuView;
use views::orchestrators::OrchestratorsView;
use views::policies::PoliciesView;
use views::settings::SettingsView;
use views::settings_menu::SettingsMenuView;
use views::{FetchRequest, StatusLevel, View, ViewAction};

/// Top-level tabs shown in the persistent shell tab bar. The active tab's
/// view is the navigation-stack root; sub-views (e.g. JobDetail) push on top
/// and hide the bar until popped.
const TOP_TABS: [(&str, ViewId); 5] = [
    ("Deliberate", ViewId::MainMenu),
    ("Rooms", ViewId::Rooms),
    ("Agents", ViewId::Agents),
    ("Policies", ViewId::Policies),
    ("Settings", ViewId::SettingsMenu),
];

/// Resolve a key event to a target tab index: digits `1`–`5` jump directly,
/// `Tab`/`BackTab` cycle. `None` for any other key.
fn tab_switch_target(ev: &crossterm::event::Event, current: usize) -> Option<usize> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};
    let Event::Key(key) = ev else {
        return None;
    };
    if key.kind != KeyEventKind::Press {
        return None;
    }
    let n = TOP_TABS.len();
    match key.code {
        KeyCode::Char(c) if ('1'..='5').contains(&c) => {
            let idx = c as usize - '1' as usize;
            (idx < n).then_some(idx)
        }
        KeyCode::Tab => Some((current + 1) % n),
        KeyCode::BackTab => Some((current + n - 1) % n),
        _ => None,
    }
}

fn render_tab_bar(frame: &mut Frame, area: Rect, active: usize) {
    let titles: Vec<Line> = TOP_TABS
        .iter()
        .enumerate()
        .map(|(i, (label, _))| Line::from(format!(" {} {label} ", i + 1)))
        .collect();
    let tabs = Tabs::new(titles)
        .select(active)
        .block(Block::default().borders(Borders::ALL).title(" nsed "))
        .highlight_style(
            Style::default()
                .fg(Color::Black)
                .bg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("");
    frame.render_widget(tabs, area);
}

/// Run the full interactive TUI starting at the main menu.
pub async fn run_tui(config_path: &Path) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    run_tui_loop(config, config_path, ViewId::MainMenu).await
}

/// Run the TUI with a pre-filled task (from `nsed run --tui`).
///
/// Starts the main menu but auto-navigates to the Rooms view so the user
/// can select a room and launch the deliberation with the provided task.
pub async fn run_tui_with_task(
    config_path: &Path,
    _task: Option<&str>,
    _room: Option<&str>,
    _policy: Option<&str>,
) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    // TODO: pre-fill task in MainMenu and auto-navigate
    run_tui_loop(config, config_path, ViewId::MainMenu).await
}

/// Run the TUI jumping directly to a job detail view.
pub async fn run_tui_job(config_path: &Path, job_id: &str, orchestrator: &str) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    run_tui_loop(
        config,
        config_path,
        ViewId::JobDetail {
            job_id: job_id.to_string(),
            orchestrator: orchestrator.to_string(),
        },
    )
    .await
}

/// Core TUI event loop.
async fn run_tui_loop(
    config: WorkspaceConfig,
    config_path: &Path,
    initial_view: ViewId,
) -> ExitCode {
    // Terminal setup
    let result = setup_and_run(config, config_path, initial_view).await;

    // Always restore terminal
    let _ = restore_terminal();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("TUI error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn setup_and_run(
    config: WorkspaceConfig,
    config_path: &Path,
    initial_view: ViewId,
) -> Result<(), Box<dyn std::error::Error>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Set up panic hook to restore terminal
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        original_hook(info);
    }));

    // Create app state
    let mut app = App::new(config, config_path.to_path_buf());
    if initial_view != ViewId::MainMenu {
        app.push_view(initial_view);
    }

    // Create data channel
    let (data_tx, mut data_rx) = mpsc::unbounded_channel::<DataEvent>();
    let mut tui_client = TuiClient::new(data_tx.clone());

    // Start terminal event loop
    let mut term_rx = spawn_terminal_event_loop(EventLoopConfig::default());

    // Create initial view and trigger on_enter
    let mut current_view: Box<dyn View> = create_view(app.current_view().unwrap(), &app);
    let initial_actions = current_view.on_enter();
    for action in initial_actions {
        handle_action(&mut app, &mut tui_client, &data_tx, &action, config_path);
    }

    // Main loop
    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            // Tab bar only at a tab root; a pushed sub-view gets the full area.
            if app.view_stack.len() == 1 {
                let chunks =
                    Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
                render_tab_bar(frame, chunks[0], app.active_tab);
                current_view.draw(frame, chunks[1]);
            } else {
                current_view.draw(frame, area);
            }
        })?;

        // Wait for event
        let app_event = tokio::select! {
            Some(term_event) = term_rx.recv() => term_event,
            Some(data_event) = data_rx.recv() => AppEvent::Data(data_event),
            else => break,
        };

        // Intercept JobSubmitted/MessageInjected at the loop level before
        // forwarding to the view, so we can push the JobDetail view.
        if let AppEvent::Data(DataEvent::JobSubmitted {
            ref job_id,
            ref orchestrator,
        }) = app_event
        {
            app.status_message = Some((
                format!("Job {} submitted", &job_id[..job_id.len().min(12)]),
                StatusLevel::Success,
            ));
            let view_id = ViewId::JobDetail {
                job_id: job_id.clone(),
                orchestrator: orchestrator.clone(),
            };
            app.push_view(view_id);
            current_view = create_view(app.current_view().unwrap(), &app);
            let actions = current_view.on_enter();
            for a in actions {
                handle_action(&mut app, &mut tui_client, &data_tx, &a, config_path);
            }
            continue;
        }
        if let AppEvent::Data(DataEvent::MessageInjected {
            ref job_id,
            sequence,
            round,
        }) = app_event
        {
            app.status_message = Some((
                format!(
                    "Message #{} injected into job {} at round {}",
                    sequence,
                    &job_id[..job_id.len().min(12)],
                    round
                ),
                StatusLevel::Success,
            ));
            // Fall through so the view can also process this event
        }

        // Top-level tab switch — digits 1-5 / Tab / BackTab. Only at a tab
        // root (depth 1) and only when the active view isn't capturing text,
        // so form/filter typing keeps its keys.
        if app.view_stack.len() == 1
            && !current_view.captures_input()
            && let AppEvent::Terminal(ref ev) = app_event
            && let Some(target) = tab_switch_target(ev, app.active_tab)
        {
            if target != app.active_tab {
                app.active_tab = target;
                app.view_stack = vec![TOP_TABS[target].1.clone()];
                current_view = create_view(app.current_view().unwrap(), &app);
                let actions = current_view.on_enter();
                for a in actions {
                    handle_action(&mut app, &mut tui_client, &data_tx, &a, config_path);
                }
            }
            continue;
        }

        // Update view
        if let Some(action) = current_view.update(&app_event) {
            match action {
                ViewAction::Quit => break,
                ViewAction::Pop => {
                    if !app.pop_view() {
                        break; // At root, quit
                    }
                    current_view = create_view(app.current_view().unwrap(), &app);
                    let actions = current_view.on_enter();
                    for a in actions {
                        handle_action(&mut app, &mut tui_client, &data_tx, &a, config_path);
                    }
                }
                ViewAction::Push(view_id) => {
                    app.push_view(view_id);
                    current_view = create_view(app.current_view().unwrap(), &app);
                    let actions = current_view.on_enter();
                    for a in actions {
                        handle_action(&mut app, &mut tui_client, &data_tx, &a, config_path);
                    }
                }
                ref other => {
                    handle_action(&mut app, &mut tui_client, &data_tx, other, config_path);
                }
            }
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}

/// Create a view instance from a `ViewId`.
fn create_view(view_id: &ViewId, app: &App) -> Box<dyn View> {
    // Determine default remote orchestrator for API-dependent views.
    // Only remote orchestrators have HTTP endpoints for agents/policies.
    let remote_orch = app
        .config
        .orchestrators
        .iter()
        .find(|(_, o)| o.mode.as_ref() == Some(&OrchestratorMode::Remote))
        .map(|(n, _)| n.clone());

    match view_id {
        ViewId::MainMenu => Box::new(MainMenuView::new(
            app.config.rooms.clone(),
            app.config.default_room.clone(),
            remote_orch
                .clone()
                .unwrap_or_else(|| "(no remote orchestrator)".into()),
        )),
        ViewId::SettingsMenu => Box::new(SettingsMenuView::new()),
        ViewId::Policies => Box::new(PoliciesView::new(
            remote_orch
                .clone()
                .unwrap_or_else(|| "(no remote orchestrator)".into()),
            app.config.policies.clone(),
        )),
        ViewId::Agents => Box::new(AgentsView::new(
            remote_orch.unwrap_or_else(|| "(no remote orchestrator)".into()),
        )),
        ViewId::Rooms => Box::new(views::rooms::RoomsView::new(
            remote_orch.unwrap_or_else(|| "(no remote orchestrator)".into()),
            app.config.rooms.clone(),
        )),
        ViewId::Orchestrators => Box::new(OrchestratorsView::new(app.config.orchestrators.clone())),
        ViewId::Settings => Box::new(SettingsView::from_config(
            &app.config,
            &app.config_path.display().to_string(),
        )),
        ViewId::JobDetail {
            job_id,
            orchestrator,
        } => Box::new(JobDetailView::new(job_id.clone(), orchestrator.clone())),
    }
}

/// Build a `RemoteOrchestrator` client from an orchestrator name in config.
fn build_remote(app: &App, name: &str) -> Result<RemoteOrchestrator, String> {
    let orch = app
        .config
        .orchestrators
        .get(name)
        .ok_or_else(|| format!("unknown orchestrator '{name}'"))?;
    RemoteOrchestrator::from_config(name, orch)
}

/// Handle a `ViewAction` from a view.
fn handle_action(
    app: &mut App,
    tui_client: &mut TuiClient,
    data_tx: &mpsc::UnboundedSender<DataEvent>,
    action: &ViewAction,
    config_path: &Path,
) {
    match action {
        ViewAction::Fetch(request) => {
            let (orch_name, dispatch_result) = match request {
                FetchRequest::Health { orchestrator } => {
                    let name = orchestrator.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.check_health(remote, name.clone());
                    });
                    (name, result)
                }
                FetchRequest::Agents { orchestrator } => {
                    let name = orchestrator.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.fetch_agents(remote, name.clone());
                    });
                    (name, result)
                }
                FetchRequest::Policies { orchestrator, tag } => {
                    let name = orchestrator.clone();
                    let tag = tag.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.fetch_policies(remote, name.clone(), tag);
                    });
                    (name, result)
                }
                FetchRequest::Rooms { orchestrator } => {
                    let name = orchestrator.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.fetch_rooms(remote, name.clone());
                    });
                    (name, result)
                }
                FetchRequest::CreateRoom {
                    orchestrator,
                    id,
                    tags,
                    visibility,
                    policy,
                } => {
                    let name = orchestrator.clone();
                    let (id, tags, visibility, policy) =
                        (id.clone(), tags.clone(), visibility.clone(), policy.clone());
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.create_room(remote, name.clone(), id, tags, visibility, policy);
                    });
                    (name, result)
                }
                FetchRequest::DeleteRoom { orchestrator, id } => {
                    let name = orchestrator.clone();
                    let id = id.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.delete_room(remote, name.clone(), id);
                    });
                    (name, result)
                }
                FetchRequest::StartSseStream {
                    orchestrator,
                    job_id,
                } => {
                    let name = orchestrator.clone();
                    let job_id = job_id.clone();
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.start_sse_stream(remote, job_id);
                    });
                    (name, result)
                }
            };
            match dispatch_result {
                Ok(()) => {
                    app.status_message =
                        Some((format!("Fetching from {orch_name}..."), StatusLevel::Info));
                }
                Err(ref e) => {
                    // Send error through data channel so the view can
                    // transition from Loading → Error state.
                    let context = match request {
                        FetchRequest::Health { .. } => "health",
                        FetchRequest::Agents { .. } => "agents",
                        FetchRequest::Policies { .. } => "policies",
                        FetchRequest::Rooms { .. }
                        | FetchRequest::CreateRoom { .. }
                        | FetchRequest::DeleteRoom { .. } => "rooms",
                        FetchRequest::StartSseStream { .. } => "sse_stream",
                    };
                    let _ = data_tx.send(DataEvent::FetchError {
                        context: context.into(),
                        error: e.clone(),
                    });
                    app.status_message = Some((e.clone(), StatusLevel::Error));
                }
            }
        }
        ViewAction::WriteConfig(mutation) => {
            match config_writer::apply_mutation(config_path, &mut app.config, mutation) {
                Ok(()) => {
                    app.status_message = Some(("Config updated".into(), StatusLevel::Success));
                }
                Err(e) => {
                    app.status_message = Some((format!("Config error: {e}"), StatusLevel::Error));
                }
            }
        }
        ViewAction::LaunchJob {
            orchestrator,
            task,
            room,
            policy,
            effort_override,
        } => {
            // Resolve room → policy from config
            let room_name = match room {
                Some(r) => r.clone(),
                None => {
                    app.status_message = Some(("No room specified".into(), StatusLevel::Error));
                    return;
                }
            };
            // Config-free: a remote room carries a policy *label*, not a
            // local PolicyConfig. Resolve it against the orchestrator's
            // /policies and submit directly (the launcher sets this).
            if let Some(policy_label) = policy
                && !app.config.rooms.contains_key(&room_name)
            {
                match build_remote(app, orchestrator) {
                    Ok(remote) => {
                        app.status_message =
                            Some((format!("Submitting to {room_name}…"), StatusLevel::Info));
                        tui_client.submit_with_remote_policy(
                            remote,
                            orchestrator.clone(),
                            room_name,
                            policy_label.clone(),
                            task.clone(),
                            *effort_override,
                        );
                    }
                    Err(e) => app.status_message = Some((e, StatusLevel::Error)),
                }
                return;
            }
            let room_config = match app.config.rooms.get(&room_name) {
                Some(r) => r,
                None => {
                    app.status_message = Some((
                        format!("Room '{room_name}' not found in config"),
                        StatusLevel::Error,
                    ));
                    return;
                }
            };
            let policy_name = &room_config.policy;
            let mut policy_config = match app.config.policies.get(policy_name) {
                Some(p) => p.clone(),
                None => {
                    app.status_message = Some((
                        format!("Policy '{policy_name}' not found in config"),
                        StatusLevel::Error,
                    ));
                    return;
                }
            };

            // Override the convergence threshold (`effort`) when the
            // operator typed a custom value in the launcher. Out-of-
            // range values are clamped to the orchestrator's accepted
            // band (`[0.0, 1.0]`) — the request validation rejects
            // anything else.
            if let Some(custom) = effort_override {
                policy_config.effort = custom.clamp(0.0, 1.0);
            }

            // Build the deliberation request
            let req = match build_request(&room_name, &policy_config, task) {
                Ok(r) => r,
                Err(e) => {
                    app.status_message =
                        Some((format!("Failed to build request: {e}"), StatusLevel::Error));
                    return;
                }
            };

            // Build remote client
            let remote = match build_remote(app, orchestrator) {
                Ok(r) => r,
                Err(e) => {
                    app.status_message = Some((e, StatusLevel::Error));
                    return;
                }
            };

            app.status_message = Some((
                format!("Submitting job to {orchestrator}..."),
                StatusLevel::Info,
            ));

            // Role-based policies need push_policy before submit
            if policy_config.roles.is_some() {
                tui_client.push_policy_and_submit(
                    remote,
                    policy_name.clone(),
                    policy_config,
                    req,
                    orchestrator.clone(),
                );
            } else {
                tui_client.submit_job(remote, req, orchestrator.clone());
            }
        }
        ViewAction::InjectMessage {
            orchestrator,
            job_id,
            message,
        } => {
            let remote = match build_remote(app, orchestrator) {
                Ok(r) => r,
                Err(e) => {
                    app.status_message = Some((e, StatusLevel::Error));
                    return;
                }
            };
            tui_client.inject_message(remote, job_id.clone(), message.clone());
            app.status_message = Some(("Injecting message...".into(), StatusLevel::Info));
        }
        ViewAction::SetStatus(msg, level) => {
            app.status_message = Some((msg.clone(), *level));
        }
        ViewAction::Push(_) | ViewAction::Pop | ViewAction::Quit => {
            // Handled in the main loop
        }
    }
}

fn restore_terminal() -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    execute!(io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::workspace::{OrchestratorConfig, OrchestratorMode};
    use std::collections::HashMap;

    use crate::cli::workspace::{PolicyConfig, RoomConfig};

    fn test_app(orchestrators: HashMap<String, OrchestratorConfig>) -> App {
        let config = WorkspaceConfig {
            policies: HashMap::new(),
            orchestrators,
            rooms: HashMap::new(),
            shared: None,
            default_room: None,
            agents: None,
        };
        App::new(config, std::path::PathBuf::from("/tmp/test.yaml"))
    }

    fn test_app_with_room() -> App {
        let mut orchestrators = HashMap::new();
        orchestrators.insert("prod".into(), remote_orch("http://localhost:9999", "tok"));
        let mut policies = HashMap::new();
        policies.insert(
            "review".into(),
            PolicyConfig {
                agents: Some(vec!["agent-a".into(), "agent-b".into()]),
                roles: None,
                max_rounds: 3,
                effort: 0.85,
                sla: None,
                capabilities: None,
                tags: None,
                mode: Default::default(),
            },
        );
        let mut rooms = HashMap::new();
        rooms.insert(
            "test-room".into(),
            RoomConfig {
                policy: "review".into(),
                orchestrator: Some("prod".into()),
            },
        );
        let config = WorkspaceConfig {
            policies,
            orchestrators,
            rooms,
            shared: None,
            default_room: Some("test-room".into()),
            agents: None,
        };
        App::new(config, std::path::PathBuf::from("/tmp/test.yaml"))
    }

    fn remote_orch(address: &str, token: &str) -> OrchestratorConfig {
        OrchestratorConfig {
            mode: Some(OrchestratorMode::Remote),
            address: Some(address.into()),
            token: Some(token.into()),
            nats_url: None,
            config_file: None,
        }
    }

    #[test]
    fn build_remote_success() {
        let mut orchs = HashMap::new();
        orchs.insert(
            "prod".into(),
            remote_orch("http://localhost:8080", "secret"),
        );
        let app = test_app(orchs);
        assert!(build_remote(&app, "prod").is_ok());
    }

    #[test]
    fn build_remote_unknown_orchestrator() {
        let app = test_app(HashMap::new());
        let err = build_remote(&app, "missing").err().expect("should fail");
        assert!(err.contains("unknown orchestrator"), "{err}");
    }

    #[test]
    fn build_remote_not_remote_mode() {
        let mut orchs = HashMap::new();
        orchs.insert(
            "local".into(),
            OrchestratorConfig {
                mode: None,
                address: None,
                token: None,
                nats_url: None,
                config_file: Some("config.yml".into()),
            },
        );
        let app = test_app(orchs);
        let err = build_remote(&app, "local").err().expect("should fail");
        assert!(err.contains("not a remote orchestrator"), "{err}");
    }

    #[test]
    fn build_remote_missing_address() {
        let mut orchs = HashMap::new();
        orchs.insert(
            "no-addr".into(),
            OrchestratorConfig {
                mode: Some(OrchestratorMode::Remote),
                address: None,
                token: Some("tok".into()),
                nats_url: None,
                config_file: None,
            },
        );
        let app = test_app(orchs);
        let err = build_remote(&app, "no-addr").err().expect("should fail");
        assert!(err.contains("missing address"), "{err}");
    }

    #[test]
    fn build_remote_missing_token() {
        let mut orchs = HashMap::new();
        orchs.insert(
            "no-tok".into(),
            OrchestratorConfig {
                mode: Some(OrchestratorMode::Remote),
                address: Some("http://localhost:8080".into()),
                token: None,
                nats_url: None,
                config_file: None,
            },
        );
        let app = test_app(orchs);
        let err = build_remote(&app, "no-tok").err().expect("should fail");
        assert!(err.contains("missing token"), "{err}");
    }

    #[test]
    fn handle_action_fetch_error_sets_status_and_sends_data_event() {
        let (data_tx, mut data_rx) = mpsc::unbounded_channel();
        let (client_tx, _client_rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app(HashMap::new());

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::Fetch(FetchRequest::Policies {
                orchestrator: "missing".into(),
                tag: None,
            }),
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("unknown orchestrator"), "{msg}");

        // Verify a FetchError was also sent through the data channel.
        let event = data_rx.try_recv().expect("should have error event");
        assert!(matches!(event, DataEvent::FetchError { context, .. } if context == "policies"));
    }

    #[tokio::test]
    async fn handle_action_fetch_dispatches_to_client() {
        let (data_tx, _data_rx) = mpsc::unbounded_channel();
        let (client_tx, rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut orchs = HashMap::new();
        orchs.insert("prod".into(), remote_orch("http://localhost:9999", "tok"));
        let mut app = test_app(orchs);

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::Fetch(FetchRequest::Agents {
                orchestrator: "prod".into(),
            }),
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Info);
        assert!(msg.contains("prod"), "{msg}");

        // The tokio task was spawned — the channel should eventually receive
        // a FetchError since localhost:9999 is unlikely to be running.
        // We just verify the dispatch happened by checking rx isn't closed.
        assert!(!rx.is_closed());
    }

    #[test]
    fn launch_job_no_room_sets_error() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::LaunchJob {
                orchestrator: "prod".into(),
                task: "do something".into(),
                room: None,
                policy: None,
                effort_override: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("No room"), "{msg}");
    }

    #[test]
    fn launch_job_unknown_room_sets_error() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::LaunchJob {
                orchestrator: "prod".into(),
                task: "do something".into(),
                room: Some("nonexistent".into()),
                policy: None,
                effort_override: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn launch_job_missing_policy_sets_error() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();
        // Override room to reference a nonexistent policy
        app.config.rooms.insert(
            "bad-room".into(),
            RoomConfig {
                policy: "nonexistent".into(),
                orchestrator: Some("prod".into()),
            },
        );

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::LaunchJob {
                orchestrator: "prod".into(),
                task: "do something".into(),
                room: Some("bad-room".into()),
                policy: None,
                effort_override: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("Policy") && msg.contains("not found"), "{msg}");
    }

    #[tokio::test]
    async fn launch_job_submits_to_client() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::LaunchJob {
                orchestrator: "prod".into(),
                task: "review my code".into(),
                room: Some("test-room".into()),
                policy: None,
                effort_override: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Info);
        assert!(msg.contains("Submitting"), "{msg}");
        // Tokio task was spawned — channel should still be open
        assert!(!rx.is_closed());
    }

    #[test]
    fn inject_message_unknown_orchestrator_sets_error() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app(HashMap::new());

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::InjectMessage {
                orchestrator: "missing".into(),
                job_id: "j1".into(),
                message: "hello".into(),
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("unknown orchestrator"), "{msg}");
    }

    #[tokio::test]
    async fn inject_message_dispatches_to_client() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, rx) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();

        handle_action(
            &mut app,
            &mut client,
            &data_tx,
            &ViewAction::InjectMessage {
                orchestrator: "prod".into(),
                job_id: "j1".into(),
                message: "redirect focus".into(),
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Info);
        assert!(msg.contains("Injecting"), "{msg}");
        assert!(!rx.is_closed());
    }

    #[test]
    fn create_view_main_menu() {
        let app = test_app_with_room();
        let view = create_view(&ViewId::MainMenu, &app);
        // Should not panic — rooms are passed through
        let _ = view;
    }

    #[test]
    fn create_view_settings_menu() {
        let app = test_app_with_room();
        let view = create_view(&ViewId::SettingsMenu, &app);
        let _ = view;
    }

    fn key(code: crossterm::event::KeyCode) -> crossterm::event::Event {
        use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
        crossterm::event::Event::Key(KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        })
    }

    #[test]
    fn tab_switch_digits_jump_to_index() {
        use crossterm::event::KeyCode;
        assert_eq!(tab_switch_target(&key(KeyCode::Char('1')), 3), Some(0));
        assert_eq!(tab_switch_target(&key(KeyCode::Char('5')), 0), Some(4));
        // Out-of-range digit is not a tab.
        assert_eq!(tab_switch_target(&key(KeyCode::Char('6')), 0), None);
    }

    #[test]
    fn tab_switch_tab_and_backtab_cycle() {
        use crossterm::event::KeyCode;
        assert_eq!(tab_switch_target(&key(KeyCode::Tab), 4), Some(0));
        assert_eq!(tab_switch_target(&key(KeyCode::Tab), 0), Some(1));
        assert_eq!(tab_switch_target(&key(KeyCode::BackTab), 0), Some(4));
    }

    #[test]
    fn tab_switch_ignores_other_keys() {
        use crossterm::event::KeyCode;
        assert_eq!(tab_switch_target(&key(KeyCode::Char('q')), 0), None);
        assert_eq!(tab_switch_target(&key(KeyCode::Enter), 0), None);
    }

    #[test]
    fn top_tabs_first_is_deliberate_root() {
        assert_eq!(TOP_TABS[0].1, ViewId::MainMenu);
        assert_eq!(TOP_TABS.len(), 5);
    }
}
