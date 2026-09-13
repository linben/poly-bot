.PHONY: check test local local-explore local-headless local-once run probe paper tui deploy terraform-init terraform-check lambda db-up db-down history-status history-import history-grade backtest backtest-audit

# Binaries installed by `make deploy`; add history backtest with FEATURES=--features postgres.
DEPLOY_BINS ?= local tui paper
FEATURES ?=

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

# Build and install the binaries under an immutable per-commit directory,
# then point `target/deploy/current` at it. The systemd unit runs from
# `current`, so a later build never overwrites the file a running daemon
# executes; restart the unit to pick up a new deploy.
deploy:
	cargo build --release $(FEATURES) $(foreach bin,$(DEPLOY_BINS),--bin $(bin))
	sha=$$(git rev-parse --short HEAD 2>/dev/null || echo unknown); \
	dir=target/deploy/$$sha; \
	mkdir -p $$dir; \
	for bin in $(DEPLOY_BINS); do install -m 0755 target/release/$$bin $$dir/$$bin.tmp && mv $$dir/$$bin.tmp $$dir/$$bin; done; \
	ln -sfn $$sha target/deploy/current; \
	echo "deployed $$sha -> target/deploy/current"

# Exploration profile: the loosest gates validation permits, so a quiet market
# still classifies rows. Paper positions opened under it are not comparable
# with the default policy; use a separate data dir.
local-explore:
	MINIMUM_RAW_EDGE=0.01 MINIMUM_NET_EDGE=0 MINIMUM_POSITION_FRACTION=0 \
	MINIMUM_PRICE=0.10 MAXIMUM_PRICE=0.90 WATCHLIST_SOURCE_FAMILIES=1 \
	MINIMUM_SOURCE_FAMILIES=2 REQUIRE_REFERENCE_BOOK=false \
	cargo run --release --bin local -- --data-dir data-explore

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

# Optional history archive (DATABASE_URL, --features postgres). `db-up` starts
# the compose Postgres; the archive migrates itself on first connection.
db-up:
	docker compose up -d postgres

db-down:
	docker compose down

history-status:
	cargo run --release --features postgres --bin history -- status

# Archive scan files written before the database existed (idempotent).
history-import:
	cargo run --release --features postgres --bin history -- import

history-grade:
	cargo run --release --features postgres --bin history -- grade

# Replay the archive under the gates in the environment; ARGS passes flags
# such as --from 2026-09-01 --open-class watchlist --fill next-book.
backtest:
	cargo run --release --features postgres --bin backtest -- $(ARGS)

# Recompute every live frame under its archived gates; mismatches must be 0.
backtest-audit:
	cargo run --release --features postgres --bin backtest -- --audit $(ARGS)

terraform-check:
	terraform -chdir=infra fmt -check
	terraform -chdir=infra validate

terraform-init:
	terraform -chdir=infra init

lambda:
	cargo lambda build --release --arm64 --features aws --bin news-worker
