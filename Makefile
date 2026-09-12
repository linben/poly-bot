.PHONY: check test local local-once run probe paper api terraform-init terraform-check lambda

check:
	cargo fmt --check
	cargo clippy --all-targets --all-features -- -D warnings

test:
	cargo test --all-features

# Local mode: scanner loop + news loop + dashboard in one process.
local:
	cargo run --release --bin local

local-once:
	cargo run --release --bin local -- --once

# Cloud one-shot scan (what the ECS task runs).
run:
	cargo run --bin scanner

probe:
	cargo run --bin source-probe -- --adapters-only

paper:
	cargo run --bin paper -- list

api:
	cargo run --bin api

terraform-check:
	terraform -chdir=infra fmt -check
	terraform -chdir=infra validate

terraform-init:
	terraform -chdir=infra init

lambda:
	cargo lambda build --release --arm64 --features aws --bin api --bin news-worker
