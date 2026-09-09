# This file is the single source of truth for every quality gate in Mira.
#
# CI does not reimplement a single one of them: .github/workflows/ci.yml calls
# these targets and nothing else. That is the whole point — a gate that only
# exists in YAML is a gate no contributor can run, and a gate that exists in
# both places is two gates that drift. Adding a check means adding it here;
# wiring it into CI is then one line, and `make check` picks it up for free.
#
# `cargo` is not on PATH in a non-login shell on the maintainer's machine.
# Every recipe below goes through $(CARGO), so `make CARGO=$$HOME/.cargo/bin/cargo`
# works without touching your profile — but exporting PATH is nicer:
#   export PATH="$$HOME/.cargo/bin:$$PATH"

SHELL := /usr/bin/env bash
.SHELLFLAGS := -eu -o pipefail -c
.DEFAULT_GOAL := help

CARGO   ?= cargo
PYTHON  ?= python3
UI_DIR  := crates/mira/ui
BIN     := target/release/mira

# The coverage ratchet. This number is the coverage that existed when this line
# was last edited. It may only ever go up. Raising it is a one-line diff a
# reviewer can see; lowering it needs an argument in the PR body. A gate set to
# an aspiration is a gate that gets switched off the first time it goes red.
# docs/ARCHITECTURE.md and .github/workflows/ci.yml both defer to this value.
COVERAGE_MIN ?= 96.5

# MSRV. Declared in Cargo.toml as rust-version and load-bearing for the crate
# count (see crates/mira/Cargo.toml: the ratatui-vs-libc trade assumes a floor
# old enough that nobody is forced onto a newer toolchain to use Mira). A
# declared MSRV that is never compiled against is a wish, so `make msrv` builds
# with exactly it.
MSRV := $(shell $(PYTHON) -c "import re,sys; sys.stdout.write(re.search(r'rust-version\s*=\s*\"([^\"]+)\"', open('Cargo.toml').read()).group(1))")

export CARGO_TERM_COLOR ?= always

# Every tool-dependent target routes through this so a missing tool prints the
# one command that fixes it instead of `make: cargo-deny: No such file`.
define need
	@command -v $(1) >/dev/null 2>&1 || { \
		echo "error: $(1) is not installed."; \
		echo "  run: make tools     (or: $(CARGO) install --locked $(1))"; \
		exit 1; }
endef

.PHONY: help
help: ## Show this help
	@echo "Mira — quality gates. CI runs exactly these targets."
	@echo
	@grep -hE '^[a-zA-Z0-9_-]+:.*?## ' $(MAKEFILE_LIST) \
		| sort \
		| awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-16s\033[0m %s\n", $$1, $$2}'
	@echo
	@echo "  make check        runs every PR gate. Run it before you push."

# ---------------------------------------------------------------------------
# Formatting and lints
# ---------------------------------------------------------------------------

.PHONY: fmt
fmt: ## Format Rust sources in place
	$(CARGO) fmt --all

.PHONY: fmt-check
fmt-check: ## Fail if anything is unformatted
	$(CARGO) fmt --all --check

.PHONY: lint
lint: ## Clippy over the whole workspace, warnings are errors
	$(CARGO) clippy --workspace --all-targets --locked -- -D warnings

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

.PHONY: test
test: ## Unit tests + the in-process end-to-end suite
	$(CARGO) test --workspace --locked

.PHONY: coverage
coverage: ## Line coverage against the ratchet ($(COVERAGE_MIN)%)
	$(call need,cargo-llvm-cov)
	$(CARGO) llvm-cov --workspace --locked --summary-only --fail-under-lines $(COVERAGE_MIN)

.PHONY: coverage-report
coverage-report: ## Per-file coverage, worst first — what to write tests for next
	$(call need,cargo-llvm-cov)
	$(CARGO) llvm-cov --workspace --locked --summary-only

# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

.PHONY: build
build: ## Release binary (this is the artifact; see `make drift`)
	$(CARGO) build --release --locked

.PHONY: run
run: ## Run a local instance against ./mira-data
	$(CARGO) run --release --locked -- --data-dir ./mira-data

.PHONY: ui
ui: ## Build the Svelte UI into the committed crates/mira/ui/dist
	cd $(UI_DIR) && npm ci && npm test && npm run build

.PHONY: ui-check
ui-check: ui ## Build the UI and fail if the committed dist is stale
	@# dist/ is checked in and include_bytes!'d, so the bundle in git is what
	@# ships. A .svelte fix that was never rebuilt is a fix nobody gets.
	git diff --exit-code $(UI_DIR)/dist

.PHONY: doc
doc: ## rustdoc for the whole workspace, warnings are errors
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --locked

# ---------------------------------------------------------------------------
# Supply chain — the dependency budget is a product property (see CLAUDE.md)
# ---------------------------------------------------------------------------

.PHONY: audit
audit: ## cargo-deny: advisories, licences, bans, sources
	$(call need,cargo-deny)
	cargo deny --all-features check

.PHONY: unused-deps
unused-deps: ## cargo-machete: dependencies declared but never used
	$(call need,cargo-machete)
	cargo machete --with-metadata

.PHONY: deps
deps: audit unused-deps ## All supply-chain checks

.PHONY: sbom
sbom: build ## CycloneDX SBOM, one <crate>.cdx.json beside each Cargo.toml
	$(call need,cargo-cyclonedx)
	cargo cyclonedx --format json --spec-version 1.5 --output-pattern package
	@echo "wrote: $$(ls -1 *.cdx.json crates/*/*.cdx.json 2>/dev/null | tr '\n' ' ')"

# ---------------------------------------------------------------------------
# Drift — numbers the README promises, checked against the tree that ships
# ---------------------------------------------------------------------------

.PHONY: drift
drift: build ## Crate count, binary size and doc numbers still match reality
	$(PYTHON) scripts/check_drift.py

.PHONY: workflows
workflows: ## Lint the workflows, and check every CI job can block a merge
	@# actionlint is the syntax and shellcheck pass; check_ci.py is the
	@# semantic one. Neither subsumes the other: actionlint will not notice
	@# that a job nothing depends on cannot fail a PR, and check_ci.py will not
	@# notice a typo in a `${{ }}` expression.
	@command -v actionlint >/dev/null 2>&1 || { \
		echo "error: actionlint is not installed."; \
		echo "  macOS: brew install actionlint"; \
		echo "  else:  go install github.com/rhysd/actionlint/cmd/actionlint@latest"; \
		exit 1; }
	actionlint
	$(PYTHON) scripts/check_ci.py

.PHONY: print-msrv
print-msrv: ## Print the declared MSRV (CI installs the toolchain from this)
	@echo $(MSRV)

.PHONY: msrv
msrv: ## Compile with exactly the declared MSRV ($(MSRV))
	@rustup toolchain list | grep -q '^$(MSRV)' || { \
		echo "error: Rust $(MSRV) is not installed."; \
		echo "  run: rustup toolchain install $(MSRV)"; \
		exit 1; }
	$(CARGO) +$(MSRV) check --workspace --all-targets --locked

# ---------------------------------------------------------------------------
# Documentation site
# ---------------------------------------------------------------------------

VENV := .venv-docs

$(VENV)/bin/mkdocs: docs/requirements.txt
	$(PYTHON) -m venv $(VENV)
	$(VENV)/bin/pip install --quiet --disable-pip-version-check -r docs/requirements.txt

.PHONY: docs
docs: $(VENV)/bin/mkdocs ## Build the docs site; --strict, so a dead link fails
	$(VENV)/bin/mkdocs build --strict

.PHONY: docs-serve
docs-serve: $(VENV)/bin/mkdocs ## Serve the docs site with live reload
	$(VENV)/bin/mkdocs serve

# ---------------------------------------------------------------------------
# Aggregates
# ---------------------------------------------------------------------------

.PHONY: check
check: fmt-check lint test doc ui-check deps drift workflows docs coverage ## Every PR gate, in the order they fail fastest
	@echo
	@echo "all gates passed."

.PHONY: tools
tools: ## Install the cargo subcommands the gates need
	$(CARGO) install --locked cargo-deny cargo-machete cargo-llvm-cov cargo-cyclonedx

.PHONY: clean
clean: ## Remove build output and the docs virtualenv
	$(CARGO) clean
	rm -rf $(VENV) site
