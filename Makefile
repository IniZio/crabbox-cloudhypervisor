DENY_VERSION := 0.20.2
MACHETE_VERSION := 0.9.2
NEXTEST_VERSION := 0.9.148
TAPLO_VERSION := 0.10.0
TYPOS_VERSION := 1.51.1
LLVM_COV_VERSION := 0.9.1

.PHONY: help coverage setup secret audit lint format format-check fmt-check test unit ci build deploy

help:
	@echo "targets: setup secret audit lint format format-check test ci build deploy"

setup:
	@rustup show active-toolchain >/dev/null
	@cargo deny --version 2>/dev/null | grep -q '$(DENY_VERSION)' || cargo install --locked cargo-deny@$(DENY_VERSION)
	@cargo machete --version 2>/dev/null | grep -q '$(MACHETE_VERSION)' || cargo install --locked cargo-machete@$(MACHETE_VERSION)
	@cargo nextest --version 2>/dev/null | grep -q '$(NEXTEST_VERSION)' || cargo install --locked cargo-nextest@$(NEXTEST_VERSION)
	@taplo --version 2>/dev/null | grep -q '$(TAPLO_VERSION)' || cargo install --locked taplo-cli@$(TAPLO_VERSION)
	@typos --version 2>/dev/null | grep -q '$(TYPOS_VERSION)' || cargo install --locked typos-cli@$(TYPOS_VERSION)
	@echo "setup: toolchain and pinned tools present"

secret:
	@echo "secret: crabbox-cloudhypervisor holds no repo secrets; nothing to decrypt"

audit:
	cargo deny check
	cargo machete

lint:
	cargo clippy --all-targets --locked -- -D warnings
	tools/file-cap.sh
	typos
	RUSTDOCFLAGS="-D warnings" cargo doc --no-deps --document-private-items --locked

# .editorconfig is honoured through rustfmt.toml and taplo.toml.
format:
	cargo fmt --all
	taplo fmt

format-check:
	cargo fmt --all --check
	taplo fmt --check

fmt-check: format-check

unit:
	cargo nextest run --all-targets --locked
	cargo test --doc --locked

test: unit

coverage:
	cargo llvm-cov --locked

ci:
	$(MAKE) audit
	$(MAKE) lint
	$(MAKE) format-check
	$(MAKE) test

build:
	cargo build --release --locked

deploy:
	@echo "deploy: release binaries are not published yet"

# Single source of the pins for CI's install-action step.
.PHONY: print-tool-versions
print-tool-versions:
	@echo "cargo-deny@$(DENY_VERSION),cargo-machete@$(MACHETE_VERSION),cargo-nextest@$(NEXTEST_VERSION),taplo@$(TAPLO_VERSION),typos@$(TYPOS_VERSION)"
