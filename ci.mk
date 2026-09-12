# The pipeline, as targets you can run.
#
# `.github/workflows/ci.yml` is a dispatcher over this file: every `run:` in it
# is a `make ci-*` target defined here, and `scripts/check_ci.py` fails the
# build if one ever is not. So a leg going red on a runner is a leg you can
# reproduce with one command, and there is no shell in YAML for anyone to debug
# by pushing commits.
#
# The split against the Makefile is not "CI things live over here". The
# Makefile holds the **gates** — what "correct" means, and it has to mean the
# same thing on a laptop as on a runner. This file holds what a **runner**
# adds, which is three things and only three:
#
#   * which legs a given diff needs to run at all;
#   * the tool versions the gate has to agree on, as opposed to whatever you
#     happen to have installed;
#   * the grouping of gates into legs, which is a scheduling decision.
#
#   make ci            every leg a pull request runs, on this host
#   make ci-<leg>      one leg, exactly as its job runs it
#   make ci-changes    which legs a diff against CI_BASE would run
#
# `make check` is still what to run before you push: it is the subset that
# needs no second toolchain, no docker daemon and no several minutes. `make ci`
# is that plus msrv, the image scan, e2e and the release rehearsal — the four
# the Makefile header says CI adds.
#
# `release.yml` is deliberately not dispatched this way, and the line is the
# same one: the parts of it that *build* something already call `make` —
# `dist-tarball`, `dist-sbom`, `dist-sums`, `publish` — and ci.yml's
# `release-dry-run` leg rehearses all of them on every code pull request. What
# is left is `gh release create`, cosign, SLSA attestation and the verification
# that reads them back: operations against a tag that exists and a registry that
# has been written to. A `make` target for those would be a target nobody can
# run and everybody can run by accident.

# ---------------------------------------------------------------------------
# Which legs a diff needs
# ---------------------------------------------------------------------------

# The event's base sha on a runner; the merge-base with origin/main locally,
# which is what you want when you are about to open the pull request.
CI_BASE ?=

.PHONY: ci-changes
ci-changes: ## Which CI legs a diff against CI_BASE would run
	@sh scripts/ci-changes.sh $(CI_BASE)

# ---------------------------------------------------------------------------
# One target per job in ci.yml, in the same order
# ---------------------------------------------------------------------------

.PHONY: ci-meta
ci-meta: section workflows ## The `meta` leg: the checks over the checks

# The two halves of `meta` that read the pull request rather than the tree, so
# they have nothing to run against locally and take their input as a variable.
# They are here anyway, because the alternative is shell in the workflow.
.PHONY: ci-section-range
ci-section-range: ## No section sign in commit messages in RANGE
	sh scripts/check-section-sign.sh --commits "$(RANGE)"

.PHONY: ci-section-text
ci-section-text: ## No section sign in the TITLE and BODY environment variables
	@# Through the environment, never as a variable interpolated into the
	@# workflow: a pull request title is attacker-controlled text, and `${{ }}`
	@# pastes it in before any shell sees it.
	printf '%s' "$${TITLE:-}" | sh scripts/check-section-sign.sh --text "pr title"
	printf '%s' "$${BODY:-}"  | sh scripts/check-section-sign.sh --text "pr body"

.PHONY: ci-rust
ci-rust: fmt-check lint features test doc ## The `rust` leg, on the primary runner

# What the other two architectures re-run. Formatting, the feature swap and
# rustdoc are platform-independent, so running them three times would just be
# three chances to fail for the same reason — but `lint` and `test` compile the
# cfg-gated arms, and "works on x86_64" is not a statement about aarch64 when
# the product is a memory-mapped on-disk format.
.PHONY: ci-rust-portable
ci-rust-portable: lint test ## The `rust` leg, on the other architectures

.PHONY: ci-msrv
ci-msrv: ## The `msrv` leg: install the declared floor and build with it
	@# The install is part of the leg rather than a `ci-tool-` target of its
	@# own: `rustup toolchain install` is idempotent, and a floor you have to
	@# remember to install separately is a floor that gets tested against
	@# whatever you already had.
	rustup toolchain install "$(MSRV)" --profile minimal
	$(MAKE) msrv

.PHONY: ci-ui
ci-ui: ui-check ui-demo ## The `ui` leg

.PHONY: ci-coverage
ci-coverage: ## The `coverage` leg
	@# `rustup component add` rather than a tool target: it is idempotent and
	@# cargo-llvm-cov cannot run a single line without it.
	rustup component add llvm-tools-preview
	$(MAKE) coverage

# Not part of `ci`, and the only target here that another workflow calls: the
# `docs` deploy runs it after `make site` to put the measured figure under
# miradb.dev/coverage.json, which is what the README's badge reads. It lives
# beside `ci-coverage` because it needs the same component for the same reason,
# and a second copy of that rationale is a second thing to forget.
.PHONY: ci-coverage-json
ci-coverage-json: ## The coverage figure the deployed site publishes
	rustup component add llvm-tools-preview
	$(MAKE) coverage-json

.PHONY: ci-supply-chain
ci-supply-chain: deps ## The `supply-chain` leg

.PHONY: ci-drift
ci-drift: drift reference-check ## The `drift` leg

.PHONY: ci-docs
ci-docs: docs install-script ## The `docs` leg

.PHONY: ci-image
ci-image: scan-image ## The `image` leg

.PHONY: ci-e2e
ci-e2e: e2e ## The `e2e` leg

.PHONY: ci-release-dry-run
ci-release-dry-run: dist publish-dry ## The `release-dry-run` leg

.PHONY: ci-helm
ci-helm: chart ## The `helm` leg

# ---------------------------------------------------------------------------
# Everything a pull request runs
# ---------------------------------------------------------------------------

# Spelled out leg by leg rather than as `check` plus the four it omits, so this
# line and ci.yml's job list can be read against each other. Ordered
# fastest-failing first, like `check`.
#
# On a Mac `ci-e2e` refuses with a message saying so — it needs a Linux binary
# in a Linux container — and `ci-image` needs a docker daemon. That is the
# honest answer: they are the legs a laptop cannot reproduce, and knowing which
# ones those are is worth more than an aggregate that quietly skips them.
.PHONY: ci
ci: ci-meta ci-rust ci-ui ci-supply-chain ci-docs ci-helm ci-coverage ci-drift ci-msrv ci-release-dry-run ci-image ci-e2e ## Every leg a pull request runs, on this host
	@echo
	@echo "every CI leg passed."

# ---------------------------------------------------------------------------
# What the runner installs into itself
# ---------------------------------------------------------------------------
#
# Not prerequisites of anything above, on purpose: these mutate the machine,
# and a gate that silently installs a pinned binary over the one you chose is a
# gate that lies to you about what it measured. CI calls them explicitly; you
# probably want `make tools` and your own package manager.
#
# The versions are pinned here and nowhere else. Locally `make scan-image` uses
# whatever trivy you have — a developer with a newer vulnerability database is
# a developer finding more — and only the gate needs everyone to see the same
# answer.

TRIVY_VERSION        := 0.74.0
TRIVY_INSTALL_SHA256 := e00df553be558995b994758bc8995956554a16937f456dc6615b0cc411bfec7a
ACTIONLINT_VERSION   := 1.7.12
ACTIONLINT_SHA256    := 8aca8db96f1b94770f1b0d72b6dddcb1ebb8123cb3712530b08cc387b349a3d8

.PHONY: ci-tool-trivy
ci-tool-trivy: ## Install the pinned trivy (CI; locally use your own)
	@# Not aquasecurity/trivy-action. That action was compromised in March
	@# 2026 — a pushed tag, a mutated release, and every workflow with `@v0` in
	@# it ran attacker code with whatever token the job held. The lesson is not
	@# "pin the action", it is that an action is a program with your token; an
	@# install script is a program with nothing but the runner. So: the upstream
	@# script from a tag-immutable raw URL, checked against its digest before it
	@# runs. The script then verifies the release tarball against the checksums
	@# file aquasecurity signs, so the whole chain is pinned.
	curl -fsSLo install.sh \
	  "https://raw.githubusercontent.com/aquasecurity/trivy/v$(TRIVY_VERSION)/contrib/install.sh"
	echo "$(TRIVY_INSTALL_SHA256)  install.sh" | $(SHA256) -c -
	sh install.sh -b /usr/local/bin "v$(TRIVY_VERSION)"
	rm install.sh
	trivy --version

.PHONY: ci-tool-actionlint
ci-tool-actionlint: ## Install the pinned actionlint (CI; locally use your own)
	@# Not through taiki-e/install-action: it ships no actionlint manifest, and
	@# its cargo-binstall fallback cannot find one either, because actionlint is
	@# a Go program and not a crate. A release tarball checked against its
	@# published digest is the same guarantee pinning an action by SHA gives.
	curl -fsSLo actionlint.tgz \
	  "https://github.com/rhysd/actionlint/releases/download/v$(ACTIONLINT_VERSION)/actionlint_$(ACTIONLINT_VERSION)_linux_amd64.tar.gz"
	echo "$(ACTIONLINT_SHA256)  actionlint.tgz" | $(SHA256) -c -
	tar -xzf actionlint.tgz actionlint
	sudo install actionlint /usr/local/bin/actionlint
	rm actionlint actionlint.tgz

.PHONY: ci-tool-python
ci-tool-python: ## Install what the check scripts import
	pip install -r scripts/requirements.txt

.PHONY: ci-tool-chart
ci-tool-chart: ## Install the pinned chart tooling (CI; locally use your own)
	@# Both versions come out of the Makefile rather than being written a second
	@# time: a tool pinned in two places is a tool pinned in neither, and the one
	@# that generates the file the drift gate diffs has to be the version a
	@# contributor gets from `make`.
	helm plugin install https://github.com/helm-unittest/helm-unittest \
	  --version "$(HELM_UNITTEST_VERSION)"
	go install "github.com/norwoodj/helm-docs/cmd/helm-docs@v$(HELM_DOCS_VERSION)"
	@[ -z "$${GITHUB_PATH:-}" ] || echo "$$(go env GOPATH)/bin" >> "$$GITHUB_PATH"
