# quorum-rs Makefile
# Works standalone (from crate directory) and when included from the workspace root.

.PHONY: check test fmt-check clippy audit

check: ## Check quorum-rs
	@cargo check -p quorum-rs

test: ## Test quorum-rs
	@echo "Testing quorum-rs..."
	@cargo test -p quorum-rs
	@echo "quorum-rs tests passed"

fmt-check: ## Check formatting (quorum-rs)
	@cargo fmt -p quorum-rs --check

clippy: ## Run Clippy (quorum-rs, warnings = errors)
	@echo "Running Clippy..."
	@cargo clippy -p quorum-rs --all-targets -- -D warnings

audit: ## Security + license audit (cargo-deny; uses the workspace deny.toml)
	@cargo deny check
