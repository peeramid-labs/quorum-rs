//! OpenAPI documentation for the multi-agent HITL dashboard API.
//!
//! Aggregates all endpoint paths and component schemas into a single
//! [`ApiDoc`] struct that utoipa uses to generate the OpenAPI spec.

use super::{
    chat_handlers, hitl_handlers, registration_handlers, registry_handlers, status_handlers,
};
use utoipa::OpenApi;

#[derive(OpenApi)]
#[openapi(
    paths(
        // Dashboard
        super::dashboard_page,

        // Agent listing
        super::list_agents,
        super::agent_diagnostics,

        // Per-agent status & config
        status_handlers::agent_status,
        status_handlers::agent_config,

        // HITL control plane
        hitl_handlers::agent_pause,
        hitl_handlers::agent_auto_approve,
        hitl_handlers::pause_all_agents,
        hitl_handlers::auto_all_agents,
        hitl_handlers::agent_config_update,
        hitl_handlers::agent_buffer_list,
        hitl_handlers::agent_buffer_detail,
        hitl_handlers::agent_buffer_edit,
        hitl_handlers::agent_buffer_release,
        hitl_handlers::agent_buffer_reject,
        hitl_handlers::agent_buffer_stop,
        hitl_handlers::agent_buffer_unstop,

        // Chat
        chat_handlers::agent_chat,

        // Agent management (CRUD)
        registration_handlers::register_agent,
        registration_handlers::delete_agent,
        registration_handlers::replace_agent,
        registration_handlers::patch_agent,
        registration_handlers::bulk_register,

        // Registry & config
        registry_handlers::list_orchestrators,
        registry_handlers::add_orchestrator,
        registry_handlers::get_global_config,
        registry_handlers::update_global_config,
        registry_handlers::get_orchestrator_budgets,
    ),
    components(
        schemas(
            // Agent listing
            super::AgentSummary,
            super::AgentDiagnostics,

            // Agent status snapshot (SDK types)
            crate::status::AgentStatusSnapshot,
            crate::status::TaskLogEntry,
            crate::status::EventLogEntry,
            crate::status::ScoreEntry,

            // Agent config (SDK)
            crate::agents::AgentConfig,

            // Buffer types (SDK)
            crate::workers::buffer::BufferEntrySummary,
            crate::workers::buffer::BufferEntryDetail,

            // Config patch (SDK)
            crate::control_plane::ConfigPatch,

            // HITL request types
            hitl_handlers::PauseRequest,
            hitl_handlers::AutoApproveRequest,
            hitl_handlers::BufferEditRequest,

            // Chat types
            chat_handlers::ChatRole,
            chat_handlers::ChatMessage,
            chat_handlers::ChatRequest,
            chat_handlers::ChatResponse,

            // Global config
            super::GlobalConfig,
            super::GlobalConfigUpdate,

            // Orchestrator registry
            crate::orchestrator_registry::ActiveOrchestrator,
            crate::orchestrator_registry::AddOrchestratorRequest,
            registry_handlers::OrchestratorBudget,

            // Agent management (CRUD)
            registration_handlers::RegisterAgentRequest,
            registration_handlers::RegisterAgentResponse,
            registration_handlers::BulkRegisterRequest,
            registration_handlers::BulkRegisterResponse,
            registration_handlers::PatchAgentRequest,
        )
    ),
    tags(
        (name = "Dashboard", description = "HTML dashboard"),
        (name = "Agents", description = "Agent listing and summary"),
        (name = "Status", description = "Per-agent status and configuration"),
        (name = "HITL", description = "Human-in-the-loop control plane (pause, buffer, auto-approve)"),
        (name = "Chat", description = "Agent chat interface"),
        (name = "Config", description = "Global configuration"),
        (name = "Registry", description = "Orchestrator registry management"),
        (name = "Agent Management", description = "Agent CRUD — register, update, remove, bulk"),
    ),
    info(
        title = "NSED Agent HITL Dashboard API",
        version = "1.0.0",
        description = "API for the NSED Agent HITL (Human-in-the-Loop) control plane.\n\nProvides real-time agent monitoring, response buffer management,\npause/resume controls, and live configuration patching."
    ),
)]
pub struct ApiDoc;
