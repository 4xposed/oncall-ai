.PHONY: lint
lint: clippy fmt-check ## run all checks warnings-as-errors (what CI runs)

.PHONY: clippy
clippy: ## lint all targets, warnings are errors
	cargo clippy --all-targets -- -D warnings

.PHONY: fmt
fmt: ## format the codebase in place
	cargo fmt

.PHONY: fmt-check
fmt-check: ## fail if the codebase is not formatted
	cargo fmt --check

.PHONY: test
test: ## run the full test suite
	cargo test

.PHONY: build
build: ## build the debug binary
	cargo build

.PHONY: help
help: ## print this list of commands
	@grep -E '^[a-z-]+:.*##' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*## "} {printf "%-12s %s\n", $$1, $$2}'
