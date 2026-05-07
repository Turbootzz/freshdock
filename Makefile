.PHONY: ci fmt-check fmt clippy test deny build install-hooks help

# Default target: list available targets.
help:
	@echo "freshdock — local task runner"
	@echo
	@echo "  make ci             Run the full CI suite (fmt-check, clippy, test, deny)."
	@echo "  make fmt-check      cargo fmt --all -- --check"
	@echo "  make fmt            cargo fmt --all (apply formatting)"
	@echo "  make clippy         cargo clippy --all-targets --all-features --locked -- -D warnings"
	@echo "  make test           cargo test --locked --all-features"
	@echo "  make deny           cargo deny check (requires cargo-deny installed)"
	@echo "  make build          cargo build --release"
	@echo "  make install-hooks  Enable the tracked pre-push hook (.githooks/)."

# Mirror of .github/workflows/ci.yml, minus the cross-compile matrix.
ci: fmt-check clippy test deny

fmt-check:
	cargo fmt --all -- --check

fmt:
	cargo fmt --all

clippy:
	cargo clippy --all-targets --all-features --locked -- -D warnings

test:
	cargo test --locked --all-features

deny:
	cargo deny check

build:
	cargo build --release --locked

install-hooks:
	git config core.hooksPath .githooks
	@echo "Pre-push hook enabled. Disable with: git config --unset core.hooksPath"
