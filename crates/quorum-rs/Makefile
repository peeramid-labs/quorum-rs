# nsed-agent-sdk Makefile
# Works standalone (from crate directory) and when included from the workspace root.

.PHONY: check-sdk test-sdk

check-sdk: ## Check nsed-agent-sdk (MIT traits crate)
	@cargo check -p nsed-agent-sdk

test-sdk: ## Test nsed-agent-sdk
	@echo "🧪 Testing nsed-agent-sdk..."
	@cargo test -p nsed-agent-sdk
	@echo "✅ SDK tests passed"
