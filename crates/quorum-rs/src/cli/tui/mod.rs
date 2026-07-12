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
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Tabs};
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
use views::thread::ThreadView;
use views::thread_list::ThreadListView;
use views::{FetchRequest, StatusLevel, View, ViewAction};

/// Top-level tabs shown in the persistent shell tab bar. The active tab's
/// view is the navigation-stack root; sub-views (e.g. JobDetail) push on top
/// and hide the bar until popped.
const TOP_TABS: [(&str, ViewId); 2] = [
    ("Threads", ViewId::Threads),
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
        KeyCode::Char(c) if ('1'..='9').contains(&c) => {
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

/// The persistent footer text: the current model (policy) + effort, then the
/// transient status message, or key hints when idle. Pure, for testing.
fn footer_text(model: Option<&str>, effort: Option<f32>, status: Option<&str>) -> String {
    let model = model.unwrap_or("(no model)");
    let effort = effort
        .map(|e| format!("{e:.2}"))
        .unwrap_or_else(|| "default".into());
    let tail = status.unwrap_or("[n]ew  [enter]open  [tab]switch  [q]uit");
    format!(" model: {model} · effort: {effort} · {tail}")
}

fn render_footer(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    model: Option<&str>,
    effort: Option<f32>,
) {
    let color = match app.status_message.as_ref().map(|(_, l)| l) {
        Some(StatusLevel::Error) => Color::Red,
        Some(StatusLevel::Success) => Color::Green,
        Some(StatusLevel::Info) => Color::Yellow,
        None => Color::DarkGray,
    };
    let status = app.status_message.as_ref().map(|(m, _)| m.as_str());
    let text = footer_text(model, effort, status);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(text, Style::default().fg(color)))),
        area,
    );
}

/// Run the full interactive TUI starting at the main menu.
pub async fn run_tui(config_path: &Path) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default_for_view(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    run_tui_loop(config, config_path, ViewId::Threads).await
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
    let config = match WorkspaceConfig::load_or_remote_default_for_view(config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Error loading config: {e}");
            return ExitCode::FAILURE;
        }
    };

    // TODO: pre-fill task in MainMenu and auto-navigate
    run_tui_loop(config, config_path, ViewId::Threads).await
}

/// Run the TUI jumping directly to a job detail view.
pub async fn run_tui_job(config_path: &Path, job_id: &str, orchestrator: &str) -> ExitCode {
    let config = match WorkspaceConfig::load_or_remote_default_for_view(config_path) {
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
    // Bracketed paste: a paste arrives as one `Event::Paste(text)` instead of
    // char-by-char + Enter, so pasting multi-line content (e.g. from a PDF) no
    // longer submits on the first embedded newline.
    execute!(
        stdout,
        EnterAlternateScreen,
        crossterm::event::EnableBracketedPaste
    )?;
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
    // Seed the "model" shown in the footer + used for new threads.
    app.active_model = resolve_thread_policy(&app.config, None);
    if initial_view != ViewId::Threads {
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
            // Always reserve a 1-line footer at the bottom: the current model
            // (policy) + effort, and any transient status — Claude-style.
            let split = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(area);
            let (body, footer_area) = (split[0], split[1]);
            // Tab bar only at a tab root; a pushed sub-view gets the full area.
            if app.view_stack.len() == 1 {
                let chunks =
                    Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(body);
                render_tab_bar(frame, chunks[0], app.active_tab);
                current_view.draw(frame, chunks[1]);
            } else {
                current_view.draw(frame, body);
            }
            // The active view's model (a thread's) wins over the app default.
            let model = current_view.active_model().or(app.active_model.as_deref());
            render_footer(frame, footer_area, &app, model, app.active_effort);
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
            // A thread launch's job id is now known — bind it so the reply is
            // attributed to that thread (and not to whichever job finishes first).
            if let Some(thread_id) = app.pending_thread_launch.take() {
                app.job_thread.insert(job_id.clone(), thread_id.clone());
                // Persist the launched job id on the thread so a completion that
                // lands while the TUI is closed is recoverable on reopen.
                crate::cli::thread::ThreadStore::new().set_pending_job(&thread_id, job_id);
                // Stay in the thread — the reply lands inline. Don't hijack the
                // screen with the deliberation detail (open it with Ctrl-D). But
                // still open the SSE stream so completion events flow and the
                // reply is recorded + reloaded (JobDetail's on_enter used to do
                // this; without it no JobComplete would ever arrive).
                app.status_message = Some(("Deliberating…".into(), StatusLevel::Info));
                handle_action(
                    &mut app,
                    &mut tui_client,
                    &data_tx,
                    &ViewAction::Fetch(FetchRequest::StartSseStream {
                        orchestrator: orchestrator.clone(),
                        job_id: job_id.clone(),
                    }),
                    config_path,
                );
                continue;
            }
            // Room / ad-hoc launch: jump to the live deliberation detail.
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
        // A submit failed — the pending thread launch produced no job, so drop
        // it before some later `JobSubmitted` could adopt the stale thread id.
        if let AppEvent::Data(DataEvent::FetchError { context, .. }) = &app_event
            && context == "submit"
        {
            app.pending_thread_launch = None;
        }
        // A thread's deliberation completed — record the reply, attributed by
        // job id. Falls through so JobDetail still renders completion.
        {
            let store = crate::cli::thread::ThreadStore::new();
            match record_thread_terminal(&store, &mut app.job_thread, &app_event) {
                ThreadTerminal::ReplySaved => {
                    app.status_message =
                        Some(("Reply saved to thread".into(), StatusLevel::Success));
                }
                ThreadTerminal::Failed(reason) => {
                    app.status_message =
                        Some((format!("Deliberation failed: {reason}"), StatusLevel::Error));
                }
                ThreadTerminal::NotTracked => {}
            }
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
                app.status_message = None; // clear any stale fetch/status
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
                    app.status_message = None; // don't carry a stale status across views
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
                ViewAction::OpenThreadJob {
                    ref thread_id,
                    ref orchestrator,
                } => {
                    // Resolve the thread's in-flight job and open its detail.
                    let job = app
                        .job_thread
                        .iter()
                        .find(|(_, tid)| *tid == thread_id)
                        .map(|(jid, _)| jid.clone());
                    match job {
                        Some(job_id) => {
                            app.push_view(ViewId::JobDetail {
                                job_id,
                                orchestrator: orchestrator.clone(),
                            });
                            current_view = create_view(app.current_view().unwrap(), &app);
                            let actions = current_view.on_enter();
                            for a in actions {
                                handle_action(&mut app, &mut tui_client, &data_tx, &a, config_path);
                            }
                        }
                        None => {
                            app.status_message = Some((
                                "No deliberation running for this thread".into(),
                                StatusLevel::Info,
                            ))
                        }
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
        ViewId::Threads => Box::new(ThreadListView::new(crate::cli::thread::ThreadStore::new())),
        ViewId::Thread { id } => {
            let store = crate::cli::thread::ThreadStore::new();
            let thread = match id.as_deref().and_then(|tid| store.load(tid)) {
                Some(existing) => existing,
                None => {
                    // Fresh thread: empty subject (prompts for one) + inherits
                    // the footer's active model.
                    let mut t = crate::cli::thread::Thread::new("");
                    t.active_policy = app.active_model.clone();
                    t
                }
            };
            let mut models: Vec<String> = app.config.policies.keys().cloned().collect();
            models.sort();
            Box::new(ThreadView::with_thread(
                thread,
                store,
                remote_orch.unwrap_or_else(|| "(no remote orchestrator)".into()),
                models,
            ))
        }
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

/// On a deliberation's completion, record its answer back into the thread that
/// launched it — attributed by the completing **`job_id`**, so a different job
/// Outcome of a tracked thread's terminal job event.
#[derive(Debug, PartialEq, Eq)]
enum ThreadTerminal {
    /// A successful, non-empty reply was appended to the thread.
    ReplySaved,
    /// The job was this thread's but ended without a usable reply (server-side
    /// failure or empty result). Carries the reason to surface to the operator —
    /// otherwise the "deliberating…" spinner just vanishes with no explanation.
    Failed(String),
    /// Not a completion of a tracked thread job (unrelated job / non-terminal).
    NotTracked,
}

/// Human-readable failure reason for a non-success terminal job. The server sets
/// `status` to `"failed: <reason>"`; strip the prefix for display. A `success`
/// status here means the content was empty (the only other non-usable case).
fn failure_reason(status: &str) -> String {
    let s = status.trim();
    if s.eq_ignore_ascii_case("success") {
        return "empty result".to_string();
    }
    match s.split_once(':') {
        Some((head, reason))
            if head.trim().eq_ignore_ascii_case("failed") && !reason.trim().is_empty() =>
        {
            reason.trim().to_string()
        }
        _ => s.to_string(),
    }
}

/// Record a thread's terminal job event. The reply is saved only for a
/// successful, non-empty result; the `job_thread` entry is removed either way so
/// a later unrelated `JobComplete` can never steal the slot. A failure returns
/// its reason so the loop can surface it instead of silently dropping the spinner.
fn record_thread_terminal(
    store: &crate::cli::thread::ThreadStore,
    job_thread: &mut std::collections::HashMap<String, String>,
    event: &AppEvent,
) -> ThreadTerminal {
    let AppEvent::Data(DataEvent::SseEvent(event::SseEvent::JobComplete {
        job_id,
        status,
        best_proposal_content,
        ..
    })) = event
    else {
        return ThreadTerminal::NotTracked;
    };
    let Some(thread_id) = job_thread.remove(job_id) else {
        return ThreadTerminal::NotTracked;
    };
    if !status.eq_ignore_ascii_case("success") || best_proposal_content.trim().is_empty() {
        return ThreadTerminal::Failed(failure_reason(status));
    }
    if store.append_reply(&thread_id, best_proposal_content, job_id, None) {
        ThreadTerminal::ReplySaved
    } else {
        ThreadTerminal::Failed("could not save reply to the thread store".to_string())
    }
}

/// Resolve the policy label for a roomless (thread) launch: the explicit policy
/// if given, else the default room's policy, else any configured room's policy.
/// `None` when nothing is configured to fall back to.
fn resolve_thread_policy(config: &WorkspaceConfig, explicit: Option<&str>) -> Option<String> {
    if let Some(p) = explicit {
        return Some(p.to_string());
    }
    config
        .default_room
        .as_ref()
        .and_then(|dr| config.rooms.get(dr))
        .map(|r| r.policy.clone())
        .or_else(|| config.rooms.values().next().map(|r| r.policy.clone()))
}

/// Dispatch a roomless thread launch: resolve the policy, key the deliberation
/// by the thread id (so the 409-reclaim gives the thread one live job at a
/// time), record the launching thread for reply attribution, and submit — via the
/// local-policy path when the label is a config `PolicyConfig`, otherwise via
/// the orchestrator-side policy dispatch.
#[allow(clippy::too_many_arguments)]
fn launch_thread_job(
    app: &mut App,
    tui_client: &mut TuiClient,
    orchestrator: &str,
    task: &str,
    policy: Option<&str>,
    effort_override: Option<f32>,
    thread_id: Option<&str>,
    conversation_id: Option<&str>,
    new_turn: Option<&str>,
) {
    let Some(label) = resolve_thread_policy(&app.config, policy) else {
        app.status_message = Some((
            "No policy for this thread — pick one or configure a room".into(),
            StatusLevel::Error,
        ));
        return;
    };
    let room_id = thread_id.unwrap_or("adhoc").to_string();
    let remote = match build_remote(app, orchestrator) {
        Ok(r) => r,
        Err(e) => {
            app.status_message = Some((e, StatusLevel::Error));
            return;
        }
    };
    // Remember the launching thread until its JobSubmitted arrives with the
    // job id; the loop promotes it into `job_thread` there.
    app.pending_thread_launch = thread_id.map(str::to_string);
    app.status_message = Some((format!("Submitting to {orchestrator}…"), StatusLevel::Info));

    let local = app.config.policies.get(&label).cloned();
    match local {
        Some(mut pc) => {
            if let Some(custom) = effort_override {
                pc.effort = custom.clamp(0.0, 1.0);
            }
            match build_request(&room_id, &pc, task) {
                Ok(mut req) => {
                    // The room id is per-thread, but the session key is the
                    // per-branch conversation_id so forks don't share a session.
                    if let Some(cid) = conversation_id {
                        req.conversation_id = Some(cid.to_string());
                    }
                    if let Some(nt) = new_turn {
                        req.new_turn = Some(nt.to_string());
                    }
                    if pc.roles.is_some() {
                        tui_client.push_policy_and_submit(
                            remote,
                            label,
                            pc,
                            req,
                            orchestrator.to_string(),
                        );
                    } else {
                        tui_client.submit_job(remote, req, orchestrator.to_string());
                    }
                }
                Err(e) => {
                    // No job was submitted — don't strand the pending launch.
                    app.pending_thread_launch = None;
                    app.status_message =
                        Some((format!("Failed to build request: {e}"), StatusLevel::Error))
                }
            }
        }
        None => tui_client.submit_with_remote_policy(
            remote,
            orchestrator.to_string(),
            room_id,
            label,
            task.to_string(),
            effort_override,
            conversation_id.map(str::to_string),
        ),
    }
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
                FetchRequest::ReconcileThread {
                    orchestrator,
                    job_id,
                    thread_id,
                } => {
                    let name = orchestrator.clone();
                    let (job_id, thread_id) = (job_id.clone(), thread_id.clone());
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.reconcile_thread(remote, job_id, thread_id);
                    });
                    (name, result)
                }
                FetchRequest::CancelJob {
                    orchestrator,
                    job_id,
                    thread_id,
                } => {
                    let name = orchestrator.clone();
                    let (job_id, thread_id) = (job_id.clone(), thread_id.clone());
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.cancel_job(remote, job_id, thread_id);
                    });
                    (name, result)
                }
                FetchRequest::RespondToolCall {
                    orchestrator,
                    job_id,
                    call_id,
                    result: answer,
                } => {
                    let name = orchestrator.clone();
                    let (job_id, call_id, answer) =
                        (job_id.clone(), call_id.clone(), answer.clone());
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.respond_tool_call(remote, job_id, call_id, answer);
                    });
                    (name, result)
                }
                FetchRequest::PendingToolCalls {
                    orchestrator,
                    job_id,
                    thread_id,
                } => {
                    let name = orchestrator.clone();
                    let (job_id, thread_id) = (job_id.clone(), thread_id.clone());
                    let result = build_remote(app, &name).map(|remote| {
                        tui_client.fetch_pending_tool_calls(remote, job_id, thread_id);
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
                        FetchRequest::ReconcileThread { .. } => "reconcile",
                        FetchRequest::CancelJob { .. } => "cancel",
                        FetchRequest::RespondToolCall { .. } => "answer",
                        FetchRequest::PendingToolCalls { .. } => "tool_calls",
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
            thread_id,
            conversation_id,
            new_turn,
        } => {
            // Resolve room → policy from config
            let room_name = match room {
                Some(r) => r.clone(),
                None => {
                    // Roomless (thread) launch: dispatch the thread's policy
                    // directly, keyed by the thread id.
                    launch_thread_job(
                        app,
                        tui_client,
                        orchestrator,
                        task,
                        policy.as_deref(),
                        *effort_override,
                        thread_id.as_deref(),
                        conversation_id.as_deref(),
                        new_turn.as_deref(),
                    );
                    return;
                }
            };
            // Resolve the policy label: explicit (remote room) or the local
            // room's bound policy. A label that isn't a LOCAL PolicyConfig is
            // an orchestrator-side policy (id or name) — dispatch it directly
            // via /policies. This covers remote rooms AND local rooms wired to
            // a remote orchestrator policy (the wizard's "remote policy as
            // room"), which previously failed with "Policy not found".
            let policy_label = policy
                .clone()
                .or_else(|| app.config.rooms.get(&room_name).map(|r| r.policy.clone()));
            if let Some(label) = policy_label
                && !app.config.policies.contains_key(&label)
            {
                let orch = app
                    .config
                    .rooms
                    .get(&room_name)
                    .and_then(|r| r.orchestrator.clone())
                    .unwrap_or_else(|| orchestrator.clone());
                match build_remote(app, &orch) {
                    Ok(remote) => {
                        app.status_message =
                            Some((format!("Submitting to {room_name}…"), StatusLevel::Info));
                        tui_client.submit_with_remote_policy(
                            remote,
                            orch,
                            room_name,
                            label,
                            task.clone(),
                            *effort_override,
                            conversation_id.clone(),
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
        ViewAction::Push(_)
        | ViewAction::Pop
        | ViewAction::Quit
        | ViewAction::OpenThreadJob { .. } => {
            // Handled in the main loop
        }
    }
}

fn restore_terminal() -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    execute!(
        io::stdout(),
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
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
    fn launch_thread_without_resolvable_policy_errors() {
        // Roomless (thread) launch with no policy and no rooms to fall back to.
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app(HashMap::new());

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
                thread_id: Some("thread-x".into()),
                conversation_id: None,
                new_turn: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("No policy"), "{msg}");
        assert_eq!(app.pending_thread_launch, None);
    }

    #[tokio::test]
    async fn launch_thread_falls_back_to_default_room_policy_and_records_thread() {
        // Roomless launch: policy resolves from the default room, the thread is
        // recorded as active so the loop can persist the reply on completion.
        // Async because a successful resolve reaches the (fire-and-forget) submit.
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
                thread_id: Some("thread-42".into()),
                conversation_id: None,
                new_turn: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        assert_eq!(app.pending_thread_launch.as_deref(), Some("thread-42"));
        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Info);
        assert!(msg.contains("Submitting"), "{msg}");
    }

    #[test]
    fn resolve_thread_policy_prefers_explicit() {
        let app = test_app_with_room();
        assert_eq!(
            resolve_thread_policy(&app.config, Some("custom")).as_deref(),
            Some("custom")
        );
    }

    #[test]
    fn resolve_thread_policy_falls_back_to_default_room() {
        let app = test_app_with_room();
        assert_eq!(
            resolve_thread_policy(&app.config, None).as_deref(),
            Some("review")
        );
    }

    #[test]
    fn resolve_thread_policy_none_when_nothing_configured() {
        let app = test_app(HashMap::new());
        assert_eq!(resolve_thread_policy(&app.config, None), None);
    }

    #[test]
    fn footer_shows_model_effort_and_status() {
        assert_eq!(
            footer_text(Some("nsed:review"), Some(0.7), Some("Submitting…")),
            " model: nsed:review · effort: 0.70 · Submitting…"
        );
    }

    #[test]
    fn footer_defaults_when_unset() {
        let f = footer_text(None, None, None);
        assert!(f.contains("model: (no model)"));
        assert!(f.contains("effort: default"));
        assert!(f.contains("[n]ew"), "idle footer shows hints");
    }

    fn job_complete_event(job_id: &str, status: &str, content: &str) -> AppEvent {
        AppEvent::Data(DataEvent::SseEvent(event::SseEvent::JobComplete {
            status: status.into(),
            job_id: job_id.into(),
            rounds_completed: 2,
            best_proposal_content: content.into(),
            best_proposal_score: 0.9,
            best_proposal_author: "agent-a".into(),
        }))
    }

    /// A store + a thread seeded with one user message, and a `job_thread` map
    /// binding `job_id` → that thread.
    fn seeded(
        dir: &std::path::Path,
        job_id: &str,
    ) -> (
        crate::cli::thread::ThreadStore,
        String,
        HashMap<String, String>,
    ) {
        let store = crate::cli::thread::ThreadStore::with_dir(dir.to_path_buf());
        let mut t = crate::cli::thread::Thread::new("t");
        t.push_message(crate::cli::thread::Message::now("user", "q"));
        store.save(&t).unwrap();
        let map = HashMap::from([(job_id.to_string(), t.id.clone())]);
        (store, t.id, map)
    }

    #[test]
    fn terminal_completion_appends_reply_and_removes_mapping() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, tid, mut map) = seeded(tmp.path(), "job-1");
        let saved = record_thread_terminal(
            &store,
            &mut map,
            &job_complete_event("job-1", "success", "ans"),
        );
        assert_eq!(saved, ThreadTerminal::ReplySaved);
        assert!(map.is_empty(), "mapping consumed");
        let reloaded = store.load(&tid).unwrap();
        assert_eq!(reloaded.messages.len(), 2);
        assert_eq!(reloaded.messages[1].content, "ans");
    }

    #[test]
    fn terminal_completion_of_unrelated_job_is_not_appended() {
        // Bug 1 guard: a different job finishing must NOT steal this thread's slot.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, tid, mut map) = seeded(tmp.path(), "job-thread");
        let out = record_thread_terminal(
            &store,
            &mut map,
            &job_complete_event("job-OTHER", "success", "not yours"),
        );
        assert_eq!(
            out,
            ThreadTerminal::NotTracked,
            "unrelated job id → no attribution"
        );
        assert!(map.contains_key("job-thread"), "our mapping is untouched");
        assert_eq!(
            store.load(&tid).unwrap().messages.len(),
            1,
            "no reply appended"
        );
    }

    #[test]
    fn terminal_completion_failed_status_removes_mapping_without_appending() {
        // Bug 3 guard: a failed deliberation's leftover content is not the answer.
        // The reason is surfaced (stripped of the "failed:" prefix) so the operator
        // sees WHY, instead of the spinner silently vanishing.
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, tid, mut map) = seeded(tmp.path(), "job-1");
        let out = record_thread_terminal(
            &store,
            &mut map,
            &job_complete_event("job-1", "failed: no proposals to select from", "stale"),
        );
        assert_eq!(
            out,
            ThreadTerminal::Failed("no proposals to select from".to_string())
        );
        assert!(map.is_empty());
        assert_eq!(
            store.load(&tid).unwrap().messages.len(),
            1,
            "no reply appended"
        );
    }

    #[test]
    fn terminal_completion_empty_content_reports_empty_result() {
        let tmp = tempfile::TempDir::new().unwrap();
        let (store, tid, mut map) = seeded(tmp.path(), "job-1");
        let out = record_thread_terminal(
            &store,
            &mut map,
            &job_complete_event("job-1", "success", "   "),
        );
        assert_eq!(out, ThreadTerminal::Failed("empty result".to_string()));
        assert_eq!(store.load(&tid).unwrap().messages.len(), 1);
    }

    #[test]
    fn failure_reason_strips_failed_prefix() {
        assert_eq!(failure_reason("failed: rate-limited"), "rate-limited");
        assert_eq!(failure_reason("failed"), "failed");
        assert_eq!(failure_reason("success"), "empty result");
    }

    #[test]
    fn terminal_ignores_non_completion_events() {
        let tmp = tempfile::TempDir::new().unwrap();
        let store = crate::cli::thread::ThreadStore::with_dir(tmp.path().to_path_buf());
        let mut map = HashMap::from([("job-1".to_string(), "thread-x".to_string())]);
        assert_eq!(
            record_thread_terminal(&store, &mut map, &AppEvent::Tick),
            ThreadTerminal::NotTracked
        );
        assert!(
            map.contains_key("job-1"),
            "non-terminal must not touch the map"
        );
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
                thread_id: None,
                conversation_id: None,
                new_turn: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Error);
        assert!(msg.contains("not found"), "{msg}");
    }

    #[tokio::test]
    async fn launch_job_local_room_with_remote_policy_routes_to_remote_submit() {
        let (data_tx, _) = mpsc::unbounded_channel();
        let (client_tx, _) = mpsc::unbounded_channel();
        let mut client = TuiClient::new(client_tx);
        let mut app = test_app_with_room();
        // A local room whose policy is NOT a local PolicyConfig (e.g. an
        // orchestrator-side policy id the wizard wired as a room). This must
        // dispatch server-side, not fail with a local "Policy not found" — the
        // orchestrator resolves the id/name (and reports a real miss async).
        app.config.rooms.insert(
            "remote-policy-room".into(),
            RoomConfig {
                policy: "noosphera:0v1".into(),
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
                room: Some("remote-policy-room".into()),
                policy: None,
                effort_override: None,
                thread_id: None,
                conversation_id: None,
                new_turn: None,
            },
            Path::new("/tmp/test.yaml"),
        );

        // Synchronously we only see the "submitting" notice; any failure to
        // resolve the policy arrives later as a FetchError (surfaced by the
        // view), not a local validation error.
        let (msg, level) = app.status_message.unwrap();
        assert_eq!(level, StatusLevel::Info, "{msg}");
        assert!(msg.contains("Submitting"), "{msg}");
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
                thread_id: None,
                conversation_id: None,
                new_turn: None,
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
        assert_eq!(tab_switch_target(&key(KeyCode::Char('1')), 1), Some(0));
        assert_eq!(tab_switch_target(&key(KeyCode::Char('2')), 0), Some(1));
        // Out-of-range digit (2 tabs) is not a tab.
        assert_eq!(tab_switch_target(&key(KeyCode::Char('3')), 0), None);
    }

    #[test]
    fn tab_switch_tab_and_backtab_cycle() {
        use crossterm::event::KeyCode;
        // 2 tabs now → cycle modulo 2.
        assert_eq!(tab_switch_target(&key(KeyCode::Tab), 1), Some(0));
        assert_eq!(tab_switch_target(&key(KeyCode::Tab), 0), Some(1));
        assert_eq!(tab_switch_target(&key(KeyCode::BackTab), 0), Some(1));
    }

    #[test]
    fn tab_switch_ignores_other_keys() {
        use crossterm::event::KeyCode;
        assert_eq!(tab_switch_target(&key(KeyCode::Char('q')), 0), None);
        assert_eq!(tab_switch_target(&key(KeyCode::Enter), 0), None);
    }

    #[test]
    fn top_tabs_are_inbox_and_settings() {
        assert_eq!(TOP_TABS[0].1, ViewId::Threads);
        assert_eq!(TOP_TABS[0].0, "Threads");
        assert_eq!(TOP_TABS[1].0, "Settings");
        assert_eq!(TOP_TABS.len(), 2);
        // Room / Policies / Agents are no longer top tabs — they live under
        // Settings so the inbox is the front door.
        assert!(!TOP_TABS.iter().any(|(_, v)| *v == ViewId::MainMenu));
        assert!(!TOP_TABS.iter().any(|(_, v)| *v == ViewId::Policies));
    }
}
