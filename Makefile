.PHONY: check test local local-headless local-once run probe paper tui terraform-init terraform-check lambda

check:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

# Local mode: scanner loop + news loop + terminal UI in one process.
local:
	cargo run --release --bin local

# Local mode without the terminal UI (what the systemd unit runs).
local-headless:
	cargo run --release --bin local -- --headless

local-once:
	cargo run --release --bin local -- --once

# Cloud one-shot scan (what the ECS task runs).
run:
	cargo run --bin scanner

probe:
	cargo run --bin source-probe -- --adapters-only

paper:
	cargo run --bin paper -- list

# Attach the terminal UI to the configured store (RUN_MODE=local or cloud).
tui:
	cargo run --release --bin tui

terraform-check:
	terraform -chdir=infra fmt -check
	terraform -chdir=infra validate

terraform-init:
	terraform -chdir=infra init

lambda:
	cargo lambda build --release --arm64 --features aws --bin news-worker
