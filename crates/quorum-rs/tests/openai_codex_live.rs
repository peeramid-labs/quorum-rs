use async_openai::types::{
    ChatCompletionRequestMessage, ChatCompletionRequestUserMessage,
    ChatCompletionRequestUserMessageContent,
};
use quorum_rs::agents::AgentConfig;
use quorum_rs::llms::{AiModel, OpenAICodexAuthStore, OpenAICodexModel, RequestConfig};

#[tokio::test]
async fn live_openai_codex_gpt55_smoke() {
    if std::env::var("QUORUM_OPENAI_CODEX_LIVE").ok().as_deref() != Some("1") {
        eprintln!("skipping live OpenAI OAuth test; set QUORUM_OPENAI_CODEX_LIVE=1");
        return;
    }

    let store = OpenAICodexAuthStore::default().expect("auth store path");
    if store.read().expect("read auth store").is_none() {
        store
            .import_from_codex_cli()
            .expect("optional Codex CLI auth import");
    }

    let mut agent = AgentConfig::default();
    agent.name = "openai-oauth-live".to_string();
    agent.model_name =
        std::env::var("QUORUM_OPENAI_CODEX_LIVE_MODEL").unwrap_or_else(|_| "gpt-5.5".to_string());
    agent.max_tokens = 64;
    agent.use_streaming = false;

    let model = OpenAICodexModel::new(None, store);
    let response = model
        .chat_completion(
            &agent,
            RequestConfig {
                messages: vec![ChatCompletionRequestMessage::User(
                    ChatCompletionRequestUserMessage {
                        content: ChatCompletionRequestUserMessageContent::Text(
                            "Reply with exactly: QUORUM_OPENAI_CODEX_OK".to_string(),
                        ),
                        name: None,
                    },
                )],
                tools: None,
                tool_choice: None,
                presence_penalty: None,
            },
        )
        .await
        .expect("live Codex Responses call");

    let content = response
        .response
        .choices
        .first()
        .and_then(|choice| choice.message.content.as_deref())
        .expect("live Codex response should include a first choice with content");
    assert!(
        content.contains("QUORUM_OPENAI_CODEX_OK"),
        "unexpected live response: {content:?}"
    );
}
