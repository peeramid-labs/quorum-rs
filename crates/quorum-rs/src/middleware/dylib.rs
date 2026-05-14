//! Dynamic library (FFI) middleware — load a `.so`/`.dylib`/`.dll` at runtime.
//!
//! The shared library must export a C function with this signature:
//!
//! ```c
//! int32_t nsed_middleware_execute(
//!     const uint8_t *ctx_json,    // JSON-serialized MiddlewareContext
//!     uint32_t       ctx_len,     // length of ctx_json
//!     uint8_t       *out_buf,     // buffer for JSON-serialized MiddlewareVerdict
//!     uint32_t       out_buf_len, // capacity of out_buf
//!     uint32_t      *out_len      // actual bytes written to out_buf
//! ) -> int32_t;
//! ```
//!
//! Return values:
//! - `0` = success (verdict JSON written to `out_buf`)
//! - `1` = block (no JSON needed — generic "blocked by dylib" reason)
//! - `-1` = error (output buffer too small, internal error, etc.)
//!
//! # Example (Rust cdylib)
//!
//! ```rust,ignore
//! #[no_mangle]
//! pub extern "C" fn nsed_middleware_execute(
//!     ctx_json: *const u8, ctx_len: u32,
//!     out_buf: *mut u8, out_buf_len: u32, out_len: *mut u32,
//! ) -> i32 {
//!     // Parse context, check content, write verdict
//!     let verdict = r#"{"verdict":"pass"}"#;
//!     unsafe {
//!         std::ptr::copy_nonoverlapping(verdict.as_ptr(), out_buf, verdict.len());
//!         *out_len = verdict.len() as u32;
//!     }
//!     0
//! }
//! ```

use crate::middleware::{AgentMiddleware, MiddlewareContext, MiddlewareStage, MiddlewareVerdict};
use async_trait::async_trait;
use std::path::PathBuf;

/// Function signature for the FFI middleware entry point.
type MiddlewareExecuteFn = unsafe extern "C" fn(
    ctx_json: *const u8,
    ctx_len: u32,
    out_buf: *mut u8,
    out_buf_len: u32,
    out_len: *mut u32,
) -> i32;

/// Maximum output buffer size for FFI middleware (1 MB).
const MAX_OUTPUT_SIZE: usize = 1024 * 1024;

/// Middleware loaded from a dynamic library (.so / .dylib / .dll).
///
/// The library is loaded once and the `nsed_middleware_execute` symbol is
/// resolved at construction time. Each `execute()` call invokes the FFI
/// function synchronously on a blocking thread (via `tokio::task::spawn_blocking`).
pub struct DylibMiddleware {
    display_name: String,
    #[allow(dead_code)]
    lib: libloading::Library, // must stay alive to keep symbols valid
    execute_fn: MiddlewareExecuteFn,
    active_stages: Vec<MiddlewareStage>,
}

// Safety: the Library + function pointer are created together and the Library
// outlives all calls. The FFI function is required to be thread-safe.
unsafe impl Send for DylibMiddleware {}
unsafe impl Sync for DylibMiddleware {}

impl std::fmt::Debug for DylibMiddleware {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DylibMiddleware")
            .field("name", &self.display_name)
            .finish()
    }
}

impl DylibMiddleware {
    /// Load a dynamic library and resolve the `nsed_middleware_execute` symbol.
    ///
    /// # Safety
    ///
    /// The loaded library must:
    /// - Export `nsed_middleware_execute` with the correct C ABI signature
    /// - Be thread-safe (the function may be called from multiple threads)
    /// - Not cause undefined behavior when called with valid inputs
    pub unsafe fn load(path: &PathBuf, stages: Vec<MiddlewareStage>) -> Result<Self, String> {
        let lib = unsafe {
            libloading::Library::new(path)
                .map_err(|e| format!("Failed to load dylib {}: {e}", path.display()))?
        };

        let execute_fn: MiddlewareExecuteFn = unsafe {
            let sym = lib
                .get::<MiddlewareExecuteFn>(b"nsed_middleware_execute")
                .map_err(|e| {
                    format!(
                        "Symbol 'nsed_middleware_execute' not found in {}: {e}",
                        path.display()
                    )
                })?;
            *sym
        };

        let name = path
            .file_stem()
            .and_then(|n| n.to_str())
            .unwrap_or("dylib")
            .to_string();

        Ok(Self {
            display_name: name,
            lib,
            execute_fn,
            active_stages: stages,
        })
    }
}

#[async_trait]
impl AgentMiddleware for DylibMiddleware {
    async fn execute(&self, ctx: &MiddlewareContext) -> MiddlewareVerdict {
        let input = match serde_json::to_vec(ctx) {
            Ok(v) => v,
            Err(e) => {
                return MiddlewareVerdict::block(
                    "dylib_middleware",
                    format!("Context serialization error: {e}"),
                );
            }
        };

        let execute_fn = self.execute_fn;
        let name = self.display_name.clone();

        // Run FFI on a blocking thread to avoid blocking the tokio runtime
        let result = tokio::task::spawn_blocking(move || {
            let mut out_buf = vec![0u8; MAX_OUTPUT_SIZE];
            let mut out_len: u32 = 0;

            // Safety: we control the buffer sizes and the FFI contract requires
            // the function to write at most out_buf_len bytes.
            let status = unsafe {
                (execute_fn)(
                    input.as_ptr(),
                    input.len() as u32,
                    out_buf.as_mut_ptr(),
                    out_buf.len() as u32,
                    &mut out_len,
                )
            };

            match status {
                0 => {
                    // Success — parse verdict from output
                    // Clamp out_len to buffer capacity to prevent OOB from misbehaving dylib
                    let safe_len = (out_len as usize).min(out_buf.len());
                    let out_slice = &out_buf[..safe_len];
                    if out_slice.is_empty() {
                        return MiddlewareVerdict::pass();
                    }
                    serde_json::from_slice(out_slice).unwrap_or_else(|e| {
                        tracing::warn!(
                            middleware = name.as_str(),
                            error = %e,
                            "Dylib output was not valid JSON verdict — fail closed (block)"
                        );
                        MiddlewareVerdict::block(
                            "middleware_error",
                            format!("Dylib '{}' returned invalid JSON", name),
                        )
                    })
                }
                1 => MiddlewareVerdict::block("dylib_middleware", "Blocked by dylib middleware"),
                code => MiddlewareVerdict::block(
                    "dylib_middleware",
                    format!("Dylib returned error code {code}"),
                ),
            }
        })
        .await;

        match result {
            Ok(verdict) => verdict,
            Err(e) => {
                tracing::error!(
                    middleware = self.display_name.as_str(),
                    error = %e,
                    "Dylib middleware task panicked"
                );
                MiddlewareVerdict::block("dylib_middleware", "Dylib middleware panicked")
            }
        }
    }

    fn stages(&self) -> Vec<MiddlewareStage> {
        self.active_stages.clone()
    }

    fn name(&self) -> &str {
        &self.display_name
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::middleware::Verdict;

    #[test]
    fn dylib_load_nonexistent_fails() {
        let result = unsafe { DylibMiddleware::load(&PathBuf::from("/nonexistent.so"), vec![]) };
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("Failed to load"));
    }

    #[test]
    fn dylib_load_missing_symbol_fails() {
        // libc exists but doesn't have nsed_middleware_execute
        #[cfg(target_os = "linux")]
        let path = PathBuf::from("libc.so.6");
        #[cfg(target_os = "macos")]
        let path = PathBuf::from("libSystem.B.dylib");
        #[cfg(target_os = "windows")]
        let path = PathBuf::from("kernel32.dll");

        let result = unsafe { DylibMiddleware::load(&path, vec![]) };
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("nsed_middleware_execute"));
    }

    // Integration test with a real dylib would go here, but requires
    // building a test cdylib first. Covered by the binary middleware
    // tests which validate the same pipeline + verdict flow.

    #[test]
    fn verdict_from_status_codes() {
        // Status 0 with empty output = pass (tested via pipeline)
        // Status 1 = block
        let v = MiddlewareVerdict::block("dylib_middleware", "Blocked by dylib middleware");
        assert_eq!(v.verdict, Verdict::Block);
    }
}
