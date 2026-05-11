//! # llm-repair
//!
//! Utilities for repairing malformed JSON output from LLMs and extracting
//! tool calls from unstructured text.
//!
//! It is designed to be robust against common failure modes of smaller models,
//! such as truncation, invalid escapes (e.g. LaTeX), and "conversational"
//! formatting.

pub mod extraction;
pub mod history;
pub mod repair;
pub mod utils;

// Re-export key functions for convenience
pub use extraction::{
    extract_evaluations_from_markdown, extract_proposal_from_markdown, extract_python_tool_calls,
    extract_xml_tool_calls, heuristic_json_tool_calls,
};
pub use history::{pair_orphan_tool_calls, stub_tool_response};
pub use repair::{
    repair_aggressive_escapes, repair_conversational_response, repair_invalid_escapes,
    repair_tool_calls, repair_truncated_json, sanitize_json_string_lossy,
};
pub use utils::clean_json_string;
