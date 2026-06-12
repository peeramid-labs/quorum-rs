// ── Post-init guidance ─────────────────────────────────────────────────────

use comfy_table::{Attribute, Cell, Color, Table};

use super::presets::{AgentSlot, fmt_price};
use crate::brand;

/// Shell-quote a value for safe pasting into a terminal command.
///
/// Wraps the value in single quotes and escapes any embedded single quotes
/// using the `'\''` idiom (end-quote, escaped quote, re-open quote).
/// Returns the value unquoted if it contains only shell-safe characters.
pub(super) fn shell_quote(s: &str) -> String {
    // TODO(slop): add test for new `if` branch (no paired test file in this patch)
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.:/@".contains(c))
    {
        return s.to_string();
    }
    // Single-quote the value, escaping embedded single quotes
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Print info the workspace owner should share with worker node operators.
pub(super) fn print_share_with_workers(app_port: u16, worker_secret: &str) {
    let quoted_token = shell_quote(worker_secret);
    println!("  {} Share with agent runners:", brand::dim("4."));
    println!();
    println!(
        "       Orchestrator URL:  {}",
        brand::teal(&format!("http://<this-host>:{app_port}"))
    );
    println!("       Worker token:      {}", brand::teal(worker_secret));
    println!();
    println!("       {}", brand::dim("They run:"));
    println!(
        "       {}",
        brand::teal(&format!(
            "nsed-agent init --orchestrator http://<this-host>:{app_port} --auth {quoted_token}"
        ))
    );
    println!();
}

/// Print a curl command to list connected agents.
pub(super) fn print_list_agents(app_port: u16, api_secret: &str) {
    let auth_header = format!("Authorization: Bearer {api_secret}");
    let quoted_header = shell_quote(&auth_header);
    println!("  {} List connected agents:", brand::dim("5."));
    println!();
    println!(
        "       {}",
        brand::teal(&format!("curl -s http://localhost:{app_port}/agents \\"))
    );
    println!("       {}", brand::teal(&format!("  -H {quoted_header}")));
    println!();
}

/// Print a ready-to-paste curl command for submitting the first deliberation.
pub(super) fn print_submit_guidance(app_port: u16, api_secret: &str, agents: &[AgentSlot]) {
    let auth_header = format!("Authorization: Bearer {api_secret}");
    let quoted_header = shell_quote(&auth_header);
    let agent_names: Vec<String> = agents.iter().map(|a| a.name.clone()).collect();
    let names_json = serde_json::to_string(&agent_names).unwrap_or_else(|_| "[]".to_string());
    // Shell-escape the JSON body before embedding in the single-quoted
    // example. Names containing apostrophes (e.g. "O'Brien") would
    // otherwise terminate the single-quoted string and silently
    // mangle the curl payload.
    let quoted_body = shell_quote(&format!(
        r#"{{"room_id": "test", "user_query": "Hello world", "deliberation_rounds": 2, "agent_names": {names_json}}}"#
    ));

    println!("  {} Submit your first deliberation:", brand::dim("6."));
    println!();
    println!(
        "       {}",
        brand::teal(&format!(
            "curl -X POST http://localhost:{app_port}/deliberation \\"
        ))
    );
    println!(
        "       {}",
        brand::teal(&format!("  -H {quoted_header} \\"))
    );
    println!(
        "       {}",
        brand::teal("  -H 'Content-Type: application/json' \\")
    );
    println!("       {}", brand::teal(&format!("  -d {quoted_body}")));
    println!();
}

/// Print security notice explaining the current auth model.
pub(super) fn print_security_notice() {
    brand::section("security");
    brand::warn("Two tokens are generated in .env:");
    println!();
    println!(
        "       {} admin token (TOKENS__0) — full access, keep private",
        brand::orange("•")
    );
    println!(
        "       {} worker token (TOKENS__1) — safe to share with agent runners",
        brand::teal("•")
    );
    println!();
    println!(
        "       {}",
        brand::dim("The worker token can submit jobs, view results, list agents,")
    );
    println!(
        "       {}",
        brand::dim("and stream events. It cannot delete jobs or access admin")
    );
    println!(
        "       {}",
        brand::dim("endpoints — those require the admin role.")
    );
    println!();
    println!(
        "       {}",
        brand::dim("• Keep .env out of version control (already gitignored)")
    );
    println!(
        "       {}",
        brand::dim("• Use TLS (HTTPS) in production to protect tokens in transit")
    );
    println!(
        "       {}",
        brand::dim("• Rotate tokens by updating their SECRET values in .env")
    );
    println!();
}

// ── Cost estimate table ────────────────────────────────────────────────────

pub(super) fn print_pricing_table(agents: &[AgentSlot]) {
    brand::section("pricing summary");
    println!(
        "  {}",
        brand::dim("Prices are USD per million tokens ($/Mtok)")
    );

    let mut table = Table::new();
    table.load_preset(comfy_table::presets::UTF8_BORDERS_ONLY);
    table.set_header(vec![
        Cell::new("#").add_attribute(Attribute::Bold),
        Cell::new("persona").add_attribute(Attribute::Bold),
        Cell::new("model").add_attribute(Attribute::Bold),
        Cell::new("provider").add_attribute(Attribute::Bold),
        Cell::new("input $/Mtok").add_attribute(Attribute::Bold),
        Cell::new("output $/Mtok").add_attribute(Attribute::Bold),
    ]);

    // TODO(slop): add test for new `for` branch (no paired test file in this patch)
    for (i, agent) in agents.iter().enumerate() {
        let inp_str = fmt_price(agent.input_price);
        let out_str = fmt_price(agent.output_price);
        // Truncate long model names for display (char-aware for Unicode safety)
        let model_display = {
            let count = agent.model_name.chars().count();
            // TODO(slop): add test for new `if` branch (no paired test file in this patch)
            if count > 30 {
                let tail: String = agent.model_name.chars().skip(count - 29).collect();
                format!("…{tail}")
            } else {
                agent.model_name.clone()
            }
        };
        // TODO(slop): add test for new `match` branch (no paired test file in this patch)
        let inp_cell = match agent.input_price {
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            Some(v) if v > 0.0 => Cell::new(&inp_str).fg(Color::AnsiValue(208)),
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            _ => Cell::new(&inp_str),
        };
        // TODO(slop): add test for new `match` branch (no paired test file in this patch)
        let out_cell = match agent.output_price {
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            Some(v) if v > 0.0 => Cell::new(&out_str).fg(Color::AnsiValue(208)),
            // TODO(slop): add test for new `match_arm` branch (no paired test file in this patch)
            _ => Cell::new(&out_str),
        };
        table.add_row(vec![
            Cell::new(format!("{}", i + 1)),
            Cell::new(&agent.name).fg(Color::AnsiValue(135)), // purple
            Cell::new(&model_display),
            Cell::new(&agent.provider_id),
            inp_cell,
            out_cell,
        ]);
    }

    // TODO(slop): add test for new `for` branch (no paired test file in this patch)
    for line in table.to_string().lines() {
        println!("  {line}");
    }
    println!();
}
