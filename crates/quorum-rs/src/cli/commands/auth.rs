//! Provider authentication helpers.

use crate::llms::openai_codex::{OpenAICodexAuthStore, login_and_store_openai_codex_device_code};
use std::process::ExitCode;

pub async fn login_openai_codex() -> ExitCode {
    let store = match OpenAICodexAuthStore::default() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    println!("Starting OpenAI ChatGPT/Codex OAuth device login.");
    println!("Auth file: {}", store.path().display());

    let result = login_and_store_openai_codex_device_code(&store, |prompt| async move {
        println!();
        println!("Open this URL in your browser:");
        println!("  {}", prompt.verification_url);
        println!();
        println!("Enter this code:");
        println!("  {}", prompt.user_code);
        println!();
        println!("Waiting for browser authorization...");
    })
    .await;

    match result {
        Ok(tokens) => {
            let account = tokens.account_id.as_deref().unwrap_or("<unknown>");
            println!("OpenAI Codex auth saved. ChatGPT account: {account}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

pub fn status() -> ExitCode {
    let store = match OpenAICodexAuthStore::default() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    match store.read() {
        Ok(Some(auth)) => {
            let account = auth.tokens.account_id.as_deref().unwrap_or("<unknown>");
            println!("OpenAI Codex: configured");
            println!("  path: {}", store.path().display());
            println!("  account: {account}");
            ExitCode::SUCCESS
        }
        Ok(None) => {
            println!("OpenAI Codex: not configured");
            println!("  run: quorum auth openai-codex");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}
