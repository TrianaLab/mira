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
# The load harness and the demo generator. `cargo build --release` does *not*
# build examples, which is why docs/internals/e2e.md used to name a path that
# did not exist after the build it told you to run. One spelling, here, and
# every doc points at `make build`.
LOADGEN := target/release/examples/loadgen

# The coverage ratchet. This number is the coverage that existed when this line
# was last edited. It may only ever go up. Raising it is a one-line diff a
# reviewer can see; lowering it needs an argument in the PR body. A gate set to
# an aspiration is a gate that gets switched off the first time it goes red.
# docs/architecture.md and .github/workflows/ci.yml both defer to this value.
#
# Read it off a CI log, never off a laptop: `#[cfg]` splits the tree by host, so
# the Linux runner measures a slightly different denominator than a Mac does and
# lands about a tenth of a point lower. A ratchet raised to a Mac's figure is a
# ratchet that fails on the machine that enforces it. And round the figure it
# prints *down*: the table says 96.60% for 12,517 of 12,958 lines, which is
# 96.5966%, so a ratchet of 96.60 fails against the run it was copied from.
#
# 100.00 is not reachable and chasing it makes this tree worse, so the ratchet is
# the floor instead. This Mac measures 99.3493% — 133 of 20,439 lines — and 51 of
# those are nameable in the lcov export (the rest are macro expansions llvm
# counts and lcov does not). What is in the 51:
#
#   * `term.rs`'s SIGINT/SIGTERM handler, which calls `libc::_exit`. A test that
#     executes it kills the test process, so covering it is a red build.
#   * one `unreachable!()` in `tui.rs` and ~14 `panic!()`s that are the else-arm
#     of a test's own assertion. Rewriting those as `expect_err` would move the
#     panic into std and buy the percentage, at the price of every message that
#     currently names the offset or the field that was wrong.
#   * closing braces that llvm regions separately from the block they close, and
#     I/O failure arms — a `warn!` on an unlinkable block, a FUSE mount — that
#     need root or a full volume to reach.
#
# Set a tenth of a point below the Mac figure for the host drift documented
# above, not as slack: raise it off a green Linux run in CI, where the real
# ceiling is legible.
COVERAGE_MIN ?= 99.20

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

# Same contract for the tools cargo cannot install, where the fix differs per
# tool: pass the one command that installs it. Keep the hint comma-free — make
# splits $(call) arguments on commas and a hint with one in it silently loses
# its tail.
define need_bin
	@command -v $(1) >/dev/null 2>&1 || { \
		echo "error: $(1) is not installed."; \
		echo "  run: $(2)"; \
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

.PHONY: section
section: ## Fail if the U+00A7 section sign appears anywhere authored
	@# Cheap, no toolchain, and the failure is a list of file:line — so it goes
	@# in `check` ahead of everything that compiles. See the script's header for
	@# why the character is banned rather than merely discouraged.
	@# Selftest first: every way this gate breaks makes it pass, so a green run
	@# only means something once the script has proved it can still go red.
	sh scripts/check-section-sign.sh --selftest
	sh scripts/check-section-sign.sh

.PHONY: features
features: ## Lint the optional features, which nothing else compiles
	@# `webhook-tls` is off in every other gate and in the published artifacts,
	@# so without this it is a code path that only fails for whoever turns it
	@# on. Clippy rather than a full test run: the feature swaps one HTTP
	@# connector for another, and there is no TLS endpoint in the suite to
	@# point it at.
	$(CARGO) clippy -p miradb --all-targets --locked --features webhook-tls -- -D warnings

# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------

.PHONY: test
test: ## Unit tests + the in-process end-to-end suite
	$(CARGO) test --workspace --locked
	@# cargo compiles examples during `cargo test` but does not run their
	@# `#[test]`s — an example target defaults to `test = false`. loadgen's
	@# generator invariants (a trace that crosses a service boundary, buckets
	@# that sum to their count, exemplars naming traces that exist) therefore
	@# ride behind a flag instead of in a test module, and this is what runs it.
	$(CARGO) run --quiet --locked --example loadgen -- --selftest

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
build: ## Release binary and the load harness (the artifact; see `make drift`)
	$(CARGO) build --release --locked --bin mira --example loadgen

.PHONY: run
run: ## Run a local instance against ./mira-data
	$(CARGO) run --release --locked -- --data-dir ./mira-data

# ---------------------------------------------------------------------------
# The demo — one command, no Docker, no second terminal
# ---------------------------------------------------------------------------
#
# The documented way to see Mira working used to be seven steps across two
# terminals, and three of the things you had to know were written down nowhere:
# that the generator only exists inside the repo, that a block takes a couple of
# seconds to seal so the first query comes back empty, and that the UI's default
# window is the last hour so a run that stamps everything "now" gives you two
# points and a flat list. `make demo` removes all three — it backdates the data,
# it waits for the blocks, and it is one command.

# Outside the tree on purpose: it is scratch, it can be a gigabyte, and
# `demo-clean` has to be able to remove it without thinking about what else
# might be in there.
DEMO_DIR    ?= /tmp/mira-demo
DEMO_LOG    := $(DEMO_DIR).log
# How much history to lay down. It has to stay inside the UI's default window,
# which is the last hour, or the first screen is empty for a reason nobody can
# see. Not a knob so much as that constraint written down.
DEMO_WINDOW ?= 45m
# Alerting is off unless a node is pointed at a rules file, so the demo has to
# point at one or its alert pane is empty for a reason that looks like a bug.
# This file is also the worked example docs/config.md links to, so the demo is
# what keeps it honest.
DEMO_RULES  ?= docs/e2e/alerts.kyaml

# The generator is invoked with neither `--pid` nor `--data-dir`, which is what
# switches its resident-set and cost-per-GB axes on. A benchmark's memory and
# storage lines are noise in the middle of a demo; the record counts it prints
# either way are the half worth reading.

# A port already in use is the commonest way a first run fails, and on its own it
# fails as an `Address already in use` from inside the listener setup with no
# mention of which of the two ports it was. Say it first, and name it.
define PORTCHECK
import socket, sys
busy = [p for p in (4317, 4318) if socket.socket().connect_ex(("127.0.0.1", p)) == 0]
if busy:
    ports = " and ".join(str(p) for p in busy)
    print("error: port %s already in use, so Mira cannot listen there." % ports)
    print("  something is already on it - another Mira, or a collector.")
    print("  find it with:  lsof -nP -iTCP:%s -sTCP:LISTEN" % busy[0])
    print("  then:          make demo")
    sys.exit(1)
endef
export PORTCHECK

# "Wait for the first block to seal" is not a sleep. Exports are acked after
# durability, so the honest test is the one the user is about to run: ask each
# signal for a row and stop when all three have one. A sleep would be right on
# this machine and wrong on a slower one, which is how a demo that "sometimes
# opens empty" happens.
define WAITDATA
import sys, time, urllib.error, urllib.request

CHECKS = [
    ("logs",    "/api/v1/query",          '{"signal":"logs","from":"-24h","to":"now","limit":1}',   '"rows":[]'),
    ("traces",  "/api/v1/query",          '{"signal":"traces","from":"-24h","to":"now","limit":1}', '"rows":[]'),
    ("metrics", "/api/v1/metrics/names",  '{}',                                                     '"names":[]'),
]

def ask(path, body):
    req = urllib.request.Request(
        "http://127.0.0.1:4318" + path,
        data=body.encode(),
        headers={"content-type": "application/yaml"},
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.read().decode()

left = list(CHECKS)
deadline = time.monotonic() + 60
while left and time.monotonic() < deadline:
    still = []
    for name, path, body, empty in left:
        try:
            if empty in ask(path, body):
                still.append((name, path, body, empty))
        except (urllib.error.URLError, OSError):
            still.append((name, path, body, empty))
    left = still
    if left:
        time.sleep(0.3)
if left:
    print("warning: %s still has no sealed block after 60s." % ", ".join(x[0] for x in left))
    print("  the server is up; look at the log named above before filing anything.")
endef
export WAITDATA

.PHONY: demo
demo: build ## Server + realistic telemetry + the UI, in one command. Ctrl-C stops it.
	@$(PYTHON) -c "$$PORTCHECK"
	@mkdir -p "$(DEMO_DIR)"
	@echo "==> mira on $(DEMO_DIR), log in $(DEMO_LOG)"
	@"$(BIN)" --data-dir "$(DEMO_DIR)" --alerts "$(DEMO_RULES)" >"$(DEMO_LOG)" 2>&1 & \
	pid=$$!; \
	stop() { kill $$pid 2>/dev/null || true; wait $$pid 2>/dev/null || true; \
	         printf '\nstopped. the data is still there:\n  %s\n' "$(DEMO_DIR)"; \
	         printf '  read it with no server running:  %s mira --data-dir %s\n' "$(BIN)" "$(DEMO_DIR)"; \
	         printf '  delete it with:                  make demo-clean\n'; }; \
	trap stop EXIT; trap 'exit 0' INT TERM; \
	for i in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20 21 22 23 24 25; do \
	  if $(PYTHON) -c "import urllib.request as u; u.urlopen('http://127.0.0.1:4318/health', timeout=2)" 2>/dev/null; then break; fi; \
	  if ! kill -0 $$pid 2>/dev/null; then \
	    echo "error: mira exited during startup. its log said:"; cat "$(DEMO_LOG)"; exit 1; fi; \
	  sleep 0.4; \
	done; \
	echo "==> generating $(DEMO_WINDOW) of telemetry for a four-service shop"; \
	"$(LOADGEN)" --demo --for "$(DEMO_WINDOW)"; \
	echo "==> waiting for the first block of each signal to seal"; \
	$(PYTHON) -c "$$WAITDATA"; \
	printf '\n  UI            http://localhost:4318/\n'; \
	printf '  terminal UI   %s mira --addr localhost:4318   (a alerts, d node)\n' "$(BIN)"; \
	printf '  alerts        curl -s localhost:4318/api/v1/alerts\n'; \
	printf '  a query       curl -s localhost:4318/api/v1/query -H content-type:application/json \\\n'; \
	printf '                  -d %s\n' "'"'{"signal":"traces","where":[{"attr":"exception.type","eq":"payments.CardDeclined"}],"limit":3}'"'"; \
	printf '  more data     %s --demo --for 10m\n' "$(LOADGEN)"; \
	printf '\nrunning. Ctrl-C to stop.\n'; \
	wait $$pid

.PHONY: demo-clean
demo-clean: ## Delete the demo's scratch data directory and its log
	rm -rf "$(DEMO_DIR)" "$(DEMO_LOG)"
	@echo "removed $(DEMO_DIR) — a data directory is the whole of Mira's state,"
	@echo "so that is a complete uninstall of this demo."

.PHONY: ui
ui: ## Build the Svelte UI into the committed crates/mira/ui/dist
	cd $(UI_DIR) && npm ci && npm test && npm run build

.PHONY: ui-check
ui-check: ui ## Build the UI and fail if the committed dist is stale
	@# dist/ is checked in and include_bytes!'d, so the bundle in git is what
	@# ships. A .svelte fix that was never rebuilt is a fix nobody gets.
	git diff --exit-code $(UI_DIR)/dist

.PHONY: ui-demo
ui-demo: ## Build the recorded-snapshot UI the docs site hosts at /play
	@# Not committed and not embedded: this build answers from
	@# src/lib/fixtures.json instead of from a server, so it belongs on the
	@# site and nowhere near the binary. `make site` picks it up.
	cd $(UI_DIR) && npm ci && VITE_REPLAY=1 npm run build -- --outDir dist-demo
	@# And the asset URLs point at /play, which is the one thing about this
	@# build that nothing else can check. `make site` copies the directory into
	@# site/play *after* mkdocs has run, so `mkdocs --strict` — the whole link
	@# checker — structurally cannot see it; and a bundle built with the wrong
	@# `base` does not 404, it serves a 200 whose body is an empty
	@# `<div id="app">` and two 404s in a console nobody has open. That shipped
	@# once. It is asserted here rather than in `site` because this is the half
	@# a pull request can afford to run: node, and no rustdoc.
	@grep -oE '(src|href)="/[^"]+"' $(UI_DIR)/dist-demo/index.html \
	  | sed -e 's/^[a-z]*="//' -e 's/"$$//' \
	  | while read -r u; do \
	      case "$$u" in \
	        /play/*) [ -f "$(UI_DIR)/dist-demo$${u#/play}" ] && continue ;; \
	      esac; \
	      echo "error: the recorded snapshot's index.html asks for $$u."; \
	      echo "  it is served from /play/, so every asset URL in it has to be"; \
	      echo "  /play/<file> and name a file this build produced. See the"; \
	      echo "  VITE_REPLAY branch of \`base\` in $(UI_DIR)/vite.config.js."; \
	      exit 1; \
	    done
	@echo "recorded snapshot: assets resolve under /play."

.PHONY: ui-fixtures
ui-fixtures: ## Re-record src/lib/fixtures.json from a live Mira
	@# Needs the release binary and loadgen; see the script's header for why
	@# this is a committed artefact rather than a build step.
	scripts/capture-ui-fixtures.sh

.PHONY: doc
doc: ## rustdoc for the whole workspace, warnings are errors
	@# `--document-private-items`, because this output *is* the architecture
	@# reference: the mechanisms live in `mira`'s private modules and in
	@# mira-core's internals, and a public-items-only build documents a binary
	@# crate's surface, which is `fn main`. It is also the only setting under
	@# which the tree's own intra-doc links resolve — half of them point at the
	@# private constant or helper that is the actual answer.
	RUSTDOCFLAGS="-D warnings" $(CARGO) doc --workspace --no-deps --locked --document-private-items

.PHONY: reference
reference: build doc ## Regenerate the reference pages from the code they document
	@# `build` for the binary whose `--help` is the CLI page, `doc` for the
	@# rustdoc tree the generated links are checked against.
	$(PYTHON) scripts/gen_reference.py

.PHONY: reference-check
reference-check: reference ## Fail if a committed reference page is stale
	@# Same contract as the committed UI bundle: the artefact is in git so a
	@# reader gets it without a build, and a pull request that adds a route or
	@# a config key without regenerating goes red here rather than shipping a
	@# reference page that quietly stopped being true.
	git diff --exit-code -- docs/reference docs/config.md

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
	@# No --output-pattern: cargo-cyclonedx 0.5.9 — the version release.yml pins
	@# — has no such flag and exits 1 on it, and its default naming is already
	@# <crate>.cdx.json beside each Cargo.toml. Nothing ran this target in CI
	@# until `release-dry-run` did, so the release's SBOM step was going to fail
	@# on the first tag. That is the entire argument for rehearsing the release.
	cargo cyclonedx --format json --spec-version 1.5
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

.PHONY: install-script
install-script: ## The published one-liner installer still parses, lints and runs
	@# `docs/install.sh` is a symlink to this file, so what the docs site serves
	@# at miradb.dev/install.sh is these bytes — which makes it the one script
	@# here that strangers run, unreviewed, as their first contact with the
	@# project. It had no gate at all until it broke in the field.
	@command -v shellcheck >/dev/null 2>&1 || { \
		echo "error: shellcheck is not installed."; \
		echo "  macOS: brew install shellcheck"; \
		echo "  else:  https://github.com/koalaman/shellcheck#installing"; \
		exit 1; }
	shellcheck scripts/get-mira.sh
	@# The failure shellcheck does not have an opinion about, and no runner here
	@# can reproduce: `"$${a[@]}"` on an *empty* array is an unbound variable
	@# under `set -u` on bash before 4.4. macOS ships 3.2.57 as /bin/bash and
	@# `curl ... | bash` runs it, so "a Mac with no GH_TOKEN" — the common case —
	@# exited before printing anything, while every ubuntu runner (bash 5.x)
	@# found the same line perfectly legal. Hence a grep rather than a test.
	@# The safe form contains the unsafe one as its own second half, so the
	@# safe form is deleted before looking for what is left.
	@! sed 's/\$${[A-Z_]*\[@\]+"\$${[A-Z_]*\[@\]}"}//g' scripts/get-mira.sh \
	    | grep -nE '"\$$\{[A-Z_]+\[@\]\}"' || { \
		echo "error: expand possibly-empty arrays as \$${a[@]+\"\$${a[@]}\"}."; \
		echo "  the plain quoted form aborts under \`set -u\` on bash < 4.4,"; \
		echo "  which is what macOS ships as /bin/bash and what the one-liner runs."; \
		exit 1; }
	@# And it reaches its version lookup and fails there in its own words. Port 1
	@# refuses immediately, so this needs neither the network nor a published
	@# release — and it is the whole path that broke: argv assembly, `fetch`,
	@# and the `set -o pipefail` interaction that used to swallow the message.
	@out=$$(API_URL=http://127.0.0.1:1/releases bash scripts/get-mira.sh --no-sudo 2>&1 || true); \
	case "$$out" in \
	  *"No release found"*) ;; \
	  *) echo "error: the installer did not reach its version lookup cleanly:"; \
	     printf '%s\n' "$$out" | sed 's/^/    /'; exit 1 ;; \
	esac
	@echo "installer: shellcheck clean, arrays expand safely, version lookup reached."

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
	@# mkdocs derives the URL from the filename and from nothing else, so
	@# `docs/CONFIG.md` published a shouting route in a site whose every other
	@# path is quiet — and one a reader who retypes it in the wrong case cannot
	@# reach. --strict has no opinion: the page builds, every link resolves,
	@# only the URL is wrong. Hence a gate, and before the build rather than
	@# after, because the build is the slow half.
	@bad=$$(find docs -name '*.md' | grep -E '/[^/]*[A-Z][^/]*\.md$$' || true); \
	if [ -n "$$bad" ]; then \
		echo "error: these pages publish a route with capital letters in it:"; \
		printf '%s\n' "$$bad" | sed 's/^/    /'; \
		echo "  mkdocs takes the URL from the filename. Rename to lower case and"; \
		echo "  update the links — \`git grep -l <OLD>.md\` finds every one."; \
		exit 1; \
	fi
	@# The other half of the same rename: the site's own URL, spelled out in
	@# full in a chart README, a --help string and two Rust error messages,
	@# where no link checker on this repo can see it. Those are absolute and
	@# external as far as mkdocs is concerned, so the rename above turned each
	@# of them into a 404 in a message whose whole job is to tell someone where
	@# to look.
	@# `--untracked` so a file that has not been `git add`ed yet is still
	@# checked; it keeps the standard excludes, so target/ and site/ stay out.
	@bad=$$(git grep -nE --untracked 'miradb\.dev/[A-Za-z0-9_-]*[A-Z]' || true); \
	if [ -n "$$bad" ]; then \
		echo "error: these name a site route with capital letters in it:"; \
		printf '%s\n' "$$bad" | sed 's/^/    /'; \
		echo "  every published route is lower case, so this is a 404."; \
		exit 1; \
	fi
	$(VENV)/bin/mkdocs build --strict

.PHONY: docs-serve
docs-serve: $(VENV)/bin/mkdocs ## Serve the docs site with live reload
	$(VENV)/bin/mkdocs serve

.PHONY: site
site: docs doc ui-demo ## The published site: the docs, rustdoc at /api, the UI snapshot at /play
	@# rustdoc is the API reference — miradb.dev/api — rather than a second
	@# site somewhere else, because the prose links into it by symbol and a
	@# reader following one of those links should not leave the documentation.
	@# `docs` first: mkdocs empties the output directory before it writes.
	@#
	@# /play, not /demo: `docs/demo.md` already builds to site/demo, and a `cp`
	@# over it would delete a page mkdocs believes it published — a broken link
	@# --strict cannot see, because the breakage happens after it ran.
	rm -rf site/api && cp -R target/doc site/api
	rm -rf site/play && cp -R $(UI_DIR)/dist-demo site/play
	@echo "site/ built, with $$(find site/api -name '*.html' | wc -l | tr -d ' ') rustdoc pages under /api"
	@echo "and the recorded UI snapshot under /play."

# ---------------------------------------------------------------------------
# The Helm chart
# ---------------------------------------------------------------------------
#
# One chart, one workload, and the same rule as everywhere else here: CI calls
# these targets and adds nothing of its own.

CHART := charts/mira

# The plugin is pinned because an unpinned test runner is a test suite that
# changes meaning on someone else's machine.
HELM_UNITTEST_VERSION ?= 1.0.3

.PHONY: print-helm-unittest-version
print-helm-unittest-version: ## Print the pinned helm-unittest version (CI installs it)
	@echo $(HELM_UNITTEST_VERSION)

# Pinned for the same reason, and it matters more here: helm-docs *generates*
# the file `helm-docs-check` then diffs, so an unpinned generator turns a drift
# gate into a coin flip that fails on whoever upgraded last.
HELM_DOCS_VERSION ?= 1.14.2

.PHONY: print-helm-docs-version
print-helm-docs-version: ## Print the pinned helm-docs version (CI installs it)
	@echo $(HELM_DOCS_VERSION)

.PHONY: helm-lint
helm-lint: ## helm lint the chart
	$(call need_bin,helm,brew install helm   (see https://helm.sh/docs/intro/install/))
	helm lint $(CHART)

.PHONY: helm-template
helm-template: ## Render the chart across the permutations that change its shape
	$(call need_bin,helm,brew install helm   (see https://helm.sh/docs/intro/install/))
	@# Not golden files — helm-unittest below asserts the *claims*, and a golden
	@# file asserts whitespace. This gate answers the other question: does every
	@# combination that adds or removes a resource still render at all? Each
	@# line below is one axis: no PVC, a named class, an Ingress, no account,
	@# several replicas, the durability switch, self-telemetry, and the rules.
	helm template mira $(CHART) --debug >/dev/null
	helm template mira $(CHART) --set persistence.enabled=false >/dev/null
	helm template mira $(CHART) --set persistence.storageClass=gp3 --set persistence.size=100Gi >/dev/null
	helm template mira $(CHART) --set ingress.enabled=true >/dev/null
	helm template mira $(CHART) --set serviceAccount.create=false >/dev/null
	helm template mira $(CHART) --set replicaCount=3 --set service.type=LoadBalancer >/dev/null
	helm template mira $(CHART) --set config.ingest.wal=false --set config.storage.retention=720h >/dev/null
	helm template mira $(CHART) --set config.telemetry.self=true --set config.ingest.queue=1024 >/dev/null
	helm template mira $(CHART) --values $(CHART)/ci/alerting-values.yaml >/dev/null
	helm template mira $(CHART) --values $(CHART)/ci/ephemeral-values.yaml >/dev/null

.PHONY: helm-unittest
helm-unittest: ## The chart's own test suites
	$(call need_bin,helm,brew install helm   (see https://helm.sh/docs/intro/install/))
	@helm plugin list 2>/dev/null | grep -q '^unittest' || { \
		echo "error: the helm-unittest plugin is not installed."; \
		echo "  run: helm plugin install https://github.com/helm-unittest/helm-unittest --version $(HELM_UNITTEST_VERSION)"; \
		exit 1; }
	helm unittest $(CHART)

.PHONY: helm-schema
helm-schema: ## values.schema.json parses, admits the defaults, and refuses a typo
	$(call need_bin,helm,brew install helm   (see https://helm.sh/docs/intro/install/))
	$(PYTHON) -c "import json; json.load(open('$(CHART)/values.schema.json'))"
	@# Helm validates values against the schema on every template and install,
	@# so `helm-template` above already proves the shipped defaults satisfy it.
	@# What that cannot prove is that the schema *refuses* anything: a schema
	@# with a typo'd key name, or one helm never loaded, passes that test
	@# perfectly. So assert the refusals — a closed object, an enum, a minimum,
	@# an access mode Mira cannot use, and one of Mira's own value grammars.
	@for bad in persistenc.enabled=true \
	            service.type=Bogus \
	            replicaCount=0 \
	            persistence.accessMode=ReadWriteMany \
	            config.storage.retention=1week \
	            config.ingest.queue=0; do \
		if helm template mira $(CHART) --set "$$bad" >/dev/null 2>&1; then \
			echo "error: values.schema.json accepted --set $$bad."; \
			echo "  the schema is the only thing between a typo'd value and a"; \
			echo "  cluster that installs happily with the default instead."; \
			exit 1; \
		fi; \
	done
	@echo "values.schema.json: defaults accepted, 6 bad values refused."

.PHONY: helm-docs
helm-docs: ## Regenerate charts/*/README.md from README.md.gotmpl and values.yaml
	$(call need_bin,helm-docs,brew install norwoodj/tap/helm-docs   (or: go install github.com/norwoodj/helm-docs/cmd/helm-docs@v$(HELM_DOCS_VERSION)))
	helm-docs --chart-search-root charts

.PHONY: helm-docs-check
helm-docs-check: helm-docs ## Fail if the committed chart README is stale
	@# Every chart README, not this chart's: a narrower path is a gate that
	@# rewrites a file it then does not look at, and reports success.
	git diff --exit-code -- charts/*/README.md

.PHONY: chart
chart: helm-lint helm-template helm-unittest helm-schema helm-docs-check ## Every Helm chart gate

# ---------------------------------------------------------------------------
# Aggregates
# ---------------------------------------------------------------------------

.PHONY: check
check: section fmt-check lint features test doc reference-check ui-check ui-demo deps drift workflows install-script chart docs coverage ## Every PR gate, in the order they fail fastest
	@echo
	@echo "all gates passed."

.PHONY: tools
tools: ## Install the cargo subcommands the gates need
	$(CARGO) install --locked cargo-deny cargo-machete cargo-llvm-cov cargo-cyclonedx

.PHONY: clean
clean: ## Remove build output and the docs virtualenv
	$(CARGO) clean
	rm -rf $(VENV) site

# ---------------------------------------------------------------------------
# Release artifacts — the targets the release itself calls
# ---------------------------------------------------------------------------
#
# All of this used to be shell inside .github/workflows/release.yml, which meant
# the release path was tested by releasing: a bug in the tarball layout, the
# checksum file or the SBOM name first showed up on a tag, in front of everyone.
# Here it is a gate like every other one. ci.yml's `release-dry-run` leg runs
# `make dist` on every pull request and release.yml calls the same three
# targets, so the rehearsal drives production code rather than a copy of it.
#
# VERSION and TARGET are overridable rather than derived-only because the
# release matrix cross-compiles four triples from three runners and stamps the
# tag's version. Both defaults are right locally, so `make dist` needs no args.
DIST        := dist
VERSION     ?= $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
HOST_TRIPLE  = $(shell rustc -vV | sed -n 's/^host: //p')
# cargo honours CARGO_BUILD_TARGET, which is how release.yml cross-compiles
# without `make build` ever growing a --target flag — and it is also what moves
# the output from target/release/ to target/<triple>/release/. Read it in both
# places or the tarball is empty on exactly the two cross legs.
TARGET      ?= $(or $(CARGO_BUILD_TARGET),$(HOST_TRIPLE))
DIST_BIN     = $(if $(CARGO_BUILD_TARGET),target/$(CARGO_BUILD_TARGET)/release/mira,$(BIN))
# GNU coreutils and BSD spell it differently, and the release runs on both.
SHA256       = $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo "shasum -a 256")

# The floor the README's platform table promises, and the reason release.yml's
# two linux legs pin `ubuntu-22.04` rather than taking `ubuntu-latest`:
# `zstd-sys` is compiled against the runner's glibc, so the runner picks the
# floor for the whole binary. A build on 24.04 links `__isoc23_*` at
# GLIBC_2.38 and pulls the requirement up to 2.39, which is above
# `distroless/base-nossl-debian12` (2.36) — and the symptom is not a build
# error. It is `/mira: version 'GLIBC_2.39' not found` on the container's first
# line, four minutes of the collector failing to resolve a service whose
# container has already exited, and a gate that reports "the pipeline is
# broken". So the floor is asserted on the bytes instead of trusted to a runner
# label that moves on its own.
GLIBC_FLOOR ?= 2.34

.PHONY: glibc-floor
glibc-floor: build ## The Linux binary still runs on the glibc the README promises
	@# A no-op wherever `build` did not produce an ELF — on macOS the answer is
	@# "Mach-O", which is not a failure, it is a different question.
	@head -c 4 "$(DIST_BIN)" | grep -q ELF || { \
		echo "glibc floor: $(DIST_BIN) is not ELF, so there is nothing to check on this host."; \
		exit 0; }; \
	command -v objdump >/dev/null 2>&1 || { \
		echo "error: objdump (binutils) is not installed, so the glibc floor cannot be checked."; \
		echo "  it is on every ubuntu runner and comes with the \`cc\` zstd-sys needs anyway."; \
		exit 1; }; \
	max=$$(objdump -T "$(DIST_BIN)" | sed -n 's/.*GLIBC_\([0-9][0-9.]*\).*/\1/p' | sort -V | tail -1); \
	[ -n "$$max" ] || { \
		echo "error: $(DIST_BIN) references no GLIBC_ symbol version at all."; \
		echo "  that is either a static link or a parse this target no longer understands."; \
		exit 1; }; \
	[ "$$(printf '%s\n%s\n' "$$max" "$(GLIBC_FLOOR)" | sort -V | tail -1)" = "$(GLIBC_FLOOR)" ] || { \
		echo "error: $(DIST_BIN) needs GLIBC_$$max, but the floor is $(GLIBC_FLOOR)."; \
		echo "  it will not start on RHEL 9, Amazon Linux 2023, Debian 12, Ubuntu 22.04"; \
		echo "  or the distroless base the image is built on. Build it the way"; \
		echo "  release.yml does — on ubuntu-22.04 — or move the floor in the README,"; \
		echo "  docs/install.md and GLIBC_FLOOR here, together."; \
		exit 1; }; \
	echo "glibc floor: GLIBC_$$max, at or under $(GLIBC_FLOOR)"

.PHONY: dist
dist: ## Everything a release publishes, for this host, into dist/
	@# Recursive $(MAKE) rather than prerequisites: dist-sums hashes whatever is
	@# in dist/, so it has to run last, and prerequisite order is not guaranteed
	@# under -j. Three lines beats a .NOTPARALLEL that surprises someone later.
	rm -rf $(DIST)
	$(MAKE) dist-tarball
	$(MAKE) dist-sbom
	$(MAKE) dist-sums

.PHONY: dist-tarball
dist-tarball: build glibc-floor ## Tarball the $(TARGET) binary into dist/
	@name="mira-$(VERSION)-$(TARGET)"; \
	bin="$(DIST_BIN)"; \
	if [ "$(TARGET)" = "$(HOST_TRIPLE)" ]; then "$$bin" --version; fi; \
	mkdir -p "$(DIST)/$$name"; \
	cp "$$bin" "$(DIST)/$$name/mira"; \
	cp LICENSE README.md "$(DIST)/$$name/"; \
	COPYFILE_DISABLE=1 tar -czf "$(DIST)/$$name.tar.gz" -C "$(DIST)" "$$name"; \
	rm -rf "$(DIST)/$$name"; \
	line="\`$(TARGET)\`: binary $$(( $$(wc -c < "$$bin") / 1024 )) KiB, tarball $$(( $$(wc -c < "$(DIST)/$$name.tar.gz") / 1024 )) KiB"; \
	echo "$$line"; \
	[ -z "$${GITHUB_STEP_SUMMARY:-}" ] || echo "$$line" >> "$$GITHUB_STEP_SUMMARY"
# Three things above are load-bearing and none of them are obvious:
#   * the `--version` run is a smoke test — a binary that cannot execute, or
#     that disagrees with the tag, is caught here rather than by whoever
#     downloads it. Skipped when TARGET is not the host, because the
#     cross-compiled darwin x86_64 build cannot run on the arm64 runner.
#   * LICENSE travels with the binary. Apache-2.0 section 4(a) requires it.
#   * COPYFILE_DISABLE, because macOS tar otherwise writes ._ AppleDouble
#     sidecars into the archive and they surface as junk on a Linux extract.
# The size line goes to the run summary as well as stdout: README and
# docs/architecture.md section 11 both quote a binary size, and a release that
# quietly doubles it should be visible without opening a log.

.PHONY: dist-sbom
dist-sbom: sbom ## Name the CycloneDX SBOM after the release and put it in dist/
	@# Located by directory rather than by name: `make sbom` owns
	@# cargo-cyclonedx's naming and should stay free to move the file without
	@# breaking a release five minutes into the build. It writes one per package
	@# named after the package, so the rename to `miradb` turned a `-name
	@# 'mira.cdx.json'` match into zero hits — which is how the release SBOM step
	@# would have failed on a tag. The binary crate's directory is the stable
	@# fact; what the package inside it is called is not. Renamed on the way in
	@# because `miradb.cdx.json` is not a name you can attach to three tags of a
	@# repository.
	@src=$$(find crates/mira -maxdepth 1 -name '*.cdx.json' -print -quit); \
	[ -n "$$src" ] || { echo "error: make sbom produced no crates/mira/*.cdx.json"; exit 1; }; \
	mkdir -p "$(DIST)"; \
	cp "$$src" "$(DIST)/mira-$(VERSION).cdx.json"; \
	echo "wrote $(DIST)/mira-$(VERSION).cdx.json"

.PHONY: dist-sums
dist-sums: ## SHA256SUMS over everything in dist/
	@# Hashed into a temp file outside dist/ so the file cannot list itself, and
	@# `--` rather than `./*` so the names in it stay bare: they become the
	@# subject names in the SLSA attestation release.yml raises over this file,
	@# and `./mira-<ver>.tar.gz` is a subject nobody can look up by name.
	@cd "$(DIST)"; \
	rm -f SHA256SUMS; \
	tmp=$$(mktemp); \
	$(SHA256) -- * > "$$tmp"; \
	chmod 644 "$$tmp"; \
	mv "$$tmp" SHA256SUMS; \
	cat SHA256SUMS

.PHONY: publish-dry
publish-dry: ## Rehearse the crates.io publish: package, resolve, build, stop
	$(CARGO) publish --workspace --locked --dry-run

.PHONY: publish
publish: ## Publish all three crates to crates.io. Irreversible.
	$(CARGO) publish --workspace --locked
# `--workspace` rather than three invocations in dependency order: Cargo works
# the order out from the graph and, between members, waits for the index to
# serve each one before building the next. The hand-rolled version of that is a
# retry loop against a cache nobody controls, and it is the step that fails at
# the exact moment a partial publish cannot be undone.
#
# A crates.io version is consumed forever — `cargo yank` hides it from new
# resolutions and frees nothing. So `publish-dry` runs on every code PR
# (ci.yml's release-dry-run leg) and does everything this does except the
# upload, which is the only rehearsal available for a one-shot operation.
#
# The names here are `miradb`, `miradb-core` and `miradb-proto`; the binary is
# still `mira` and so is every `use` in the tree. Cargo.toml's
# [workspace.dependencies] block says why.

# ---------------------------------------------------------------------------
# The image, and the two gates over it
# ---------------------------------------------------------------------------

SCAN_IMAGE  ?= local/mira:scan
E2E_COMPOSE := docs/e2e/compose.yaml
# Docker's spelling of the architecture, which is not uname's. The Dockerfile's
# prebuilt stage copies dist/linux/$$TARGETARCH/mira.
DOCKER_ARCH  = $(if $(filter aarch64 arm64,$(shell uname -m)),arm64,amd64)

.PHONY: dist-image
dist-image: build glibc-floor ## Build the image the way the release does — from the built binary
	@# BIN=prebuilt rather than a compile inside the Dockerfile, for the same
	@# reason release.yml does it: the bytes in the image are then the bytes in
	@# the tarball that was checksummed and attested, so `make scan-image` scans
	@# what actually ships rather than a second, unverified compile of it. It
	@# also turns a cold fat-LTO build in CI into a file copy.
	mkdir -p $(DIST)/linux/$(DOCKER_ARCH)
	cp $(DIST_BIN) $(DIST)/linux/$(DOCKER_ARCH)/mira
	docker build --build-arg BIN=prebuilt --build-arg VERSION=$(VERSION) -t $(SCAN_IMAGE) .

.PHONY: scan-image
scan-image: dist-image ## Trivy over the release image; a fixable HIGH/CRITICAL fails
	@command -v trivy >/dev/null 2>&1 || { \
		echo "error: trivy is not installed."; \
		echo "  macOS: brew install trivy"; \
		echo "  else:  https://trivy.dev/latest/getting-started/installation/"; \
		exit 1; }
	@# --ignore-unfixed because a CVE with no upstream fix is not something a
	@# rebuild can clear, and a gate nobody can make green is a gate that gets
	@# switched off. HIGH,CRITICAL because the distroless base carries a
	@# permanent tail of MEDIUM glibc findings that would drown the signal.
	trivy image --severity HIGH,CRITICAL --ignore-unfixed --exit-code 1 --no-progress $(SCAN_IMAGE)

# The three questions `make demo` waits on (WAITDATA, above), asked as
# assertions rather than as a warning. This is the only test in the tree where
# the client on the wire is not ours — the stock collector gzips by default,
# batches on its own schedule, and drops a batch permanently rather than retry
# if the server answers UNIMPLEMENTED — so a timeout here is a failed gate.
define E2EASSERT
import sys, time, urllib.error, urllib.request

CHECKS = [
    ("traces",  "/api/v1/query",         '{"signal":"traces","from":"-24h","to":"now","limit":1}', '"rows":[]'),
    ("logs",    "/api/v1/query",         '{"signal":"logs","from":"-24h","to":"now","limit":1}',   '"rows":[]'),
    ("metrics", "/api/v1/metrics/names", '{}',                                                     '"names":[]'),
]

def ask(path, body):
    req = urllib.request.Request(
        "http://127.0.0.1:4318" + path,
        data=body.encode(),
        headers={"content-type": "application/yaml"},
    )
    with urllib.request.urlopen(req, timeout=5) as r:
        return r.read().decode()

# Generous on purpose: the runner pulls two images, starts a collector, runs
# four one-shot generators and waits for a block to seal. Slow is fine here;
# never is the failure this gate is looking for.
left = list(CHECKS)
deadline = time.monotonic() + 240
while left and time.monotonic() < deadline:
    still = []
    for check in left:
        name, path, body, empty = check
        try:
            if empty in ask(path, body):
                still.append(check)
            else:
                print("  %s: came back out of Mira" % name)
        except (urllib.error.URLError, OSError):
            still.append(check)
    left = still
    if left:
        time.sleep(1)
if left:
    sys.exit("error: %s never arrived - generator -> collector -> mira -> query API is broken"
             % ", ".join(c[0] for c in left))
print("e2e: all three signals made the full trip")
endef
export E2EASSERT

.PHONY: e2e
e2e: dist-image ## docs/e2e: a stock collector in front of a real binary, asserted
	@# dist-image copies the host binary in, so on macOS the image builds happily
	@# around a Mach-O that Linux cannot exec — and the symptom is 240 seconds of
	@# silence followed by "the pipeline is broken", which it is not. Say so now.
	@head -c 4 "$(DIST_BIN)" | grep -q ELF || { \
		echo "error: $(DIST_BIN) is not a Linux binary, so the container cannot start it."; \
		echo "  this gate runs on Linux (ci.yml's e2e leg). Locally, use \`make demo\`."; \
		exit 1; }
	@# The scenario docs/internals/e2e.md section 6 documents, run as a gate. The
	@# --build-arg makes compose reuse the layers dist-image just built instead
	@# of compiling a second time inside the Dockerfile; everything else about
	@# the stack is exactly what a reader of that section types.
	docker compose -f $(E2E_COMPOSE) build --build-arg BIN=prebuilt mira
	@# Every service, not `mira otelcol`, and `ps -a` before the logs. The two
	@# things the narrow version could not show are the two that matter when
	@# this fails: which containers are still up (a name that stops resolving is
	@# a container that exited, not a network fault), and whether the generators
	@# ever reached the collector. `--tail 100` is per container, and the one
	@# line that explains the whole run is usually the container's first, so the
	@# dead one gets its log in full.
	@trap 'rc=$$?; [ $$rc -eq 0 ] || { \
	         echo "--- containers"; docker compose -f $(E2E_COMPOSE) ps -a; \
	         echo "--- mira"; docker compose -f $(E2E_COMPOSE) logs --no-color mira; \
	         echo "--- everything else"; docker compose -f $(E2E_COMPOSE) logs --no-color --tail 100; }; \
	       docker compose -f $(E2E_COMPOSE) down -v --remove-orphans >/dev/null 2>&1 || true; \
	       exit $$rc' EXIT; \
	docker compose -f $(E2E_COMPOSE) up -d; \
	$(PYTHON) -c "$$E2EASSERT"
