//! Provider authentication helpers.

use crate::llms::openai_codex::{
    ManualCodePrompt, OpenAICodexAuthStore, login_and_store_openai_codex_browser,
    login_and_store_openai_codex_device_code,
};
use std::io::Write as _;
use std::process::ExitCode;
use tokio::io::AsyncBufReadExt as _;

pub async fn login_openai_codex(device_code: bool, open_browser: bool) -> ExitCode {
    let store = match OpenAICodexAuthStore::default() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("error: {e:#}");
            return ExitCode::FAILURE;
        }
    };

    if device_code {
        return login_openai_codex_device_code(&store).await;
    }

    println!("Starting OpenAI ChatGPT/Codex OAuth browser login.");
    println!("Auth file: {}", store.path().display());

    let result = login_and_store_openai_codex_browser(
        &store,
        |prompt| async move {
            println!();
            println!("Open this URL in your browser:");
            println!("  {}", prompt.authorization_url);
            println!();
            println!(
                "Waiting for OAuth callback on http://127.0.0.1:{}/auth/callback ...",
                prompt.callback_port
            );
            if open_browser && open_url_in_browser(&prompt.authorization_url) {
                println!("Browser opened. Complete sign-in there.");
            }
        },
        |prompt| async move { read_manual_authorization_code(prompt).await },
    )
    .await;

    finish_login(result)
}

async fn login_openai_codex_device_code(store: &OpenAICodexAuthStore) -> ExitCode {
    println!("Starting OpenAI ChatGPT/Codex OAuth device login.");
    println!("Auth file: {}", store.path().display());
    println!(
        "Note: device-code login may require enabling device code authorization in ChatGPT Security Settings."
    );

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

    finish_login(result)
}

fn finish_login(result: anyhow::Result<crate::llms::openai_codex::OpenAICodexTokens>) -> ExitCode {
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

async fn read_manual_authorization_code(prompt: ManualCodePrompt) -> anyhow::Result<String> {
    println!();
    println!("{}", prompt.reason);
    println!(
        "If the browser did not finish automatically, copy the final redirect URL from the browser and paste it here."
    );
    println!("Expected redirect URI starts with:");
    println!("  {}", prompt.redirect_uri);
    println!();
    print!("Authorization code or redirect URL: ");
    std::io::stdout().flush()?;
    let mut input = String::new();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    reader.read_line(&mut input).await?;
    Ok(input)
}

fn open_url_in_browser(url: &str) -> bool {
    #[cfg(target_os = "macos")]
    let command = ("open", vec![url]);
    #[cfg(target_os = "windows")]
    let command = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let command = ("xdg-open", vec![url]);

    let (program, args) = command;
    std::process::Command::new(program)
        .args(args)
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
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
