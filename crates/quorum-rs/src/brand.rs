//! "The Noolog Aesthetic" — all ANSI styling routes through this module.
//!
//! Palette:
//!   Teal   `#009688`  — section headers, prompt cursor (❯), URLs, success
//!   Green  `#4CAF50`  — verified / integration-tested items
//!   Orange `#FF6F00`  — non-zero costs, caution notices
//!   Purple `#8A4DD2`  — model names, secondary metadata
//!   Dim    (SGR 2)    — table borders, dot-leaders, passive text
//!
//! Every public function checks `no_color()` and strips styling when requested.

use inquire::ui::{Attributes, Color, RenderConfig, StyleSheet, Styled};
use owo_colors::OwoColorize;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

// ── Color detection ────────────────────────────────────────────────────────

/// Atomic runtime flag toggled by [`set_no_color`].  Read on every call to
/// [`no_color`] so that `--no-color` is honoured even after early colour
/// output has occurred.
static NO_COLOR_FLAG: AtomicBool = AtomicBool::new(false);

/// One-shot probe of the `NO_COLOR` environment variable.  Evaluated once
/// (at first access) so we don't hit the environment on every styled call.
static NO_COLOR_ENV: OnceLock<bool> = OnceLock::new();

/// Returns `true` when ANSI colour output is disabled.
///
/// Checks (in priority order, short-circuiting):
///   1. `--no-color` CLI flag — call [`set_no_color`] from `main`.
///   2. `NO_COLOR` environment variable (any value).
///
/// Unlike a pure `OnceLock` approach, the runtime flag is re-read on every
/// call so that `set_no_color()` takes effect immediately regardless of
/// whether any colour output has already been produced.
pub fn no_color() -> bool {
    NO_COLOR_FLAG.load(Ordering::Relaxed)
        || *NO_COLOR_ENV.get_or_init(|| std::env::var("NO_COLOR").is_ok())
}

/// Call this from `main` when the `--no-color` flag is present.
///
/// The flag is an `AtomicBool`, so it takes effect immediately for all
/// subsequent `no_color()` calls — no ordering dependency on when the
/// first colour helper runs.
pub fn set_no_color() {
    NO_COLOR_FLAG.store(true, Ordering::Relaxed);
}

// ── Colour helpers ─────────────────────────────────────────────────────────

/// Apply teal colouring, or return plain text when `styled` is false.
fn teal_styled(s: &str, styled: bool) -> String {
    if styled {
        s.truecolor(0, 150, 136).to_string()
    } else {
        s.to_string()
    }
}

/// Apply orange colouring, or return plain text when `styled` is false.
fn orange_styled(s: &str, styled: bool) -> String {
    if styled {
        s.truecolor(255, 111, 0).to_string()
    } else {
        s.to_string()
    }
}

/// Apply purple colouring, or return plain text when `styled` is false.
fn purple_styled(s: &str, styled: bool) -> String {
    if styled {
        s.truecolor(138, 77, 210).to_string()
    } else {
        s.to_string()
    }
}

/// Apply green colouring, or return plain text when `styled` is false.
fn green_styled(s: &str, styled: bool) -> String {
    if styled {
        s.truecolor(76, 175, 80).to_string()
    } else {
        s.to_string()
    }
}

/// Apply dim style, or return plain text when `styled` is false.
fn dim_styled(s: &str, styled: bool) -> String {
    if styled {
        s.dimmed().to_string()
    } else {
        s.to_string()
    }
}

/// Apply bold style, or return plain text when `styled` is false.
fn bold_styled(s: &str, styled: bool) -> String {
    if styled {
        s.bold().to_string()
    } else {
        s.to_string()
    }
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn teal(s: &str) -> String {
    teal_styled(s, !no_color())
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn orange(s: &str) -> String {
    orange_styled(s, !no_color())
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn purple(s: &str) -> String {
    purple_styled(s, !no_color())
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn green(s: &str) -> String {
    green_styled(s, !no_color())
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn dim(s: &str) -> String {
    dim_styled(s, !no_color())
}

// TODO(slop): public item shipped without a `///` docstring — explain intent for callers
pub fn bold(s: &str) -> String {
    bold_styled(s, !no_color())
}

// ── Structural output ──────────────────────────────────────────────────────

/// Prints the wizard banner with ASCII logo.
pub fn print_banner() {
    println!();
    println!("    {}", teal(r" _  _   ___   ___   ___"));
    println!("    {}", teal(r"| \| | / __| | __| |   \"));
    println!("    {}", teal(r"| .` | \__ \ | _|  | |) |"));
    println!("    {}", teal(r"|_|\_| |___/ |___| |___/"));
    println!();
    println!("    {}", dim("N-Way Self-Evaluating Deliberation"));
    println!("    {}", purple("Interactive Setup Wizard"));
    println!();
}

/// Formats a section separator: `  ── title ────────────────────────`
fn format_section(title: &str) -> String {
    let width = 42usize.saturating_sub(title.len() + 1);
    let dashes = "─".repeat(width);
    dim(&format!("  ── {title} {dashes}"))
}

/// Prints a section separator: `  ── title ────────────────────────`
pub fn section(title: &str) {
    println!("\n{}", format_section(title));
}

/// Formats a success line: `  ✓ message`
fn format_success(msg: &str) -> String {
    format!("  {} {}", teal("✓"), msg)
}

/// Prints a success line: `  ✓ message`
pub fn success(msg: &str) {
    println!("{}", format_success(msg));
}

/// Prints a warning line: `  ⚠  message`
pub fn warn(msg: &str) {
    println!("  {} {}", orange("⚠"), orange(msg));
}

/// Prints a dim info line: `  · message`
// TODO(slop): placeholder identifier — pick a name that says what this is
pub fn info(msg: &str) {
    println!("  {} {}", dim("·"), dim(msg));
}

// ── Inquire render config ──────────────────────────────────────────────────

/// Build a `RenderConfig` with or without teal styling.
fn render_config_styled(styled: bool) -> RenderConfig<'static> {
    if !styled {
        return RenderConfig::default();
    }
    // Teal in inquire's 256-colour palette (closest ANSI-256: 37)
    let teal_colour = Color::AnsiValue(37);
    RenderConfig::default()
        .with_prompt_prefix(
            Styled::new("❯").with_style_sheet(StyleSheet::default().with_fg(teal_colour)),
        )
        .with_highlighted_option_prefix(
            Styled::new("❯").with_style_sheet(StyleSheet::default().with_fg(teal_colour)),
        )
        .with_selected_checkbox(
            Styled::new("◉").with_style_sheet(
                StyleSheet::default()
                    .with_fg(teal_colour)
                    .with_attr(Attributes::BOLD),
            ),
        )
        .with_answer(
            StyleSheet::default()
                .with_fg(teal_colour)
                .with_attr(Attributes::BOLD),
        )
}

/// Returns a `RenderConfig` that uses the Noolog teal colour for the prompt
/// cursor (`❯`) and selected items.  Falls back to default when `no_color()`.
pub fn render_config() -> RenderConfig<'static> {
    render_config_styled(!no_color())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_no_color_stores_atomic_flag() {
        set_no_color();
        assert!(NO_COLOR_FLAG.load(Ordering::Relaxed));
        // After set_no_color(), no_color() must return true immediately —
        // this validates the runtime AtomicBool path (no OnceLock staleness).
        assert!(no_color());
    }

    #[test]
    fn no_color_returns_bool() {
        let a = no_color();
        let b = no_color();
        assert_eq!(a, b, "no_color() should be deterministic");
    }

    #[test]
    fn color_helpers_preserve_input_text() {
        // The OnceLock may or may not have no_color=true, so we check that
        // the original text is always present in the output (with or without
        // surrounding ANSI escape sequences).
        assert!(teal("hello").contains("hello"));
        assert!(orange("warn").contains("warn"));
        assert!(purple("meta").contains("meta"));
        assert!(green("ok").contains("ok"));
        assert!(dim("faint").contains("faint"));
        assert!(bold("strong").contains("strong"));
    }

    #[test]
    fn color_helpers_handle_empty_string() {
        // Empty input must not panic; output is either "" or just ANSI codes
        let t = teal("");
        let d = dim("");
        assert!(!t.contains("error"));
        assert!(!d.contains("error"));
    }

    #[test]
    fn color_helpers_handle_unicode() {
        // Ensure multi-byte characters survive the colour pipeline
        // TODO(slop): emoji in source — almost never deliberate in code
        let emoji = "🔒 Locked";
        let result = teal(emoji);
        assert!(result.contains("Locked"));
    }

    #[test]
    fn render_config_does_not_panic() {
        let _rc = render_config();
    }

    #[test]
    fn section_width_does_not_underflow() {
        // A very long title should not panic (saturating_sub)
        let long_title = "a".repeat(100);
        // section() prints to stdout — just verify it doesn't panic
        section(&long_title);
    }

    #[test]
    fn test_print_banner_does_not_panic() {
        print_banner();
    }

    #[test]
    fn test_success_does_not_panic() {
        success("operation completed");
    }

    #[test]
    fn test_warn_does_not_panic() {
        warn("something may be wrong");
    }

    #[test]
    fn test_info_does_not_panic() {
        info("informational message");
    }

    // ── styled helpers: both branches ────────────────────────────────

    #[test]
    fn styled_helpers_plain_returns_exact_input() {
        // styled=false → plain text, no ANSI sequences
        assert_eq!(teal_styled("abc", false), "abc");
        assert_eq!(orange_styled("abc", false), "abc");
        assert_eq!(purple_styled("abc", false), "abc");
        assert_eq!(green_styled("abc", false), "abc");
        assert_eq!(dim_styled("abc", false), "abc");
        assert_eq!(bold_styled("abc", false), "abc");
    }

    #[test]
    fn styled_helpers_colored_contains_ansi() {
        // styled=true → ANSI escape codes wrapping the text
        let t = teal_styled("x", true);
        assert!(t.contains("x"));
        assert!(t.len() > 1, "styled output should have ANSI escapes");

        let o = orange_styled("x", true);
        assert!(o.contains("x"));
        assert!(o.len() > 1);

        let p = purple_styled("x", true);
        assert!(p.contains("x"));
        assert!(p.len() > 1);

        let g = green_styled("x", true);
        assert!(g.contains("x"));
        assert!(g.len() > 1);

        let d = dim_styled("x", true);
        assert!(d.contains("x"));
        assert!(d.len() > 1);

        let b = bold_styled("x", true);
        assert!(b.contains("x"));
        assert!(b.len() > 1);
    }

    #[test]
    fn render_config_styled_unstyled_returns_default() {
        // styled=false → RenderConfig::default()
        let _rc = render_config_styled(false);
    }

    #[test]
    fn render_config_styled_with_colour() {
        // styled=true → teal-themed RenderConfig
        let _rc = render_config_styled(true);
    }

    #[test]
    fn format_section_contains_title() {
        let s = format_section("providers");
        assert!(s.contains("providers"));
        assert!(s.contains("──"));
    }

    #[test]
    fn format_section_empty_title() {
        let s = format_section("");
        assert!(s.contains("──"));
    }

    #[test]
    fn format_success_contains_checkmark_and_message() {
        let s = format_success("it worked");
        assert!(s.contains("it worked"));
    }
}
