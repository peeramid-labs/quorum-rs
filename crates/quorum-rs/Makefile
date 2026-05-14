# quorum-rs Makefile
# Works standalone (from crate directory) and when included from the workspace root.

.PHONY: check test

check: ## Check quorum-rs
	@cargo check -p quorum-rs

test: ## Test quorum-rs
	@echo "Testing quorum-rs..."
	@cargo test -p quorum-rs
	@echo "quorum-rs tests passed"
