# PuffinParse developer tasks. Run `make help` for a list.
#
# Python targets expect an active virtualenv with the dev tooling installed:
#   python -m venv .venv && . .venv/bin/activate && pip install maturin ruff mypy pytest pytest-asyncio

BENCH_DATASET ?= benchmark/datasets/synthetic-v1
BENCH_MODELS  ?= reducto/standard extend/parse_performance llamaparse/cost_effective
CARGO         ?= cargo
PYTHON        ?= python
CLI           := $(CARGO) run -p puffinparse-cli --release --

.DEFAULT_GOAL := help
.PHONY: help build test test-rust test-python test-node lint fmt develop bench dataset leaderboard clean

help: ## Show this help
	@grep -hE '^[a-zA-Z_-]+:.*?## ' $(MAKEFILE_LIST) \
		| awk -F':.*?## ' '{printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'

build: ## Build the whole Rust workspace in release mode
	$(CARGO) build --workspace --release

test: test-rust test-python ## Run the Rust and Python test suites

test-rust: ## cargo test --workspace
	$(CARGO) test --workspace

test-python: ## pytest (requires `make develop` first)
	pytest python/tests -q

test-node: ## Build the Node addon (debug) and run the js/ test suite
	cd js && npm ci && npm run build:debug && npm run typecheck && npm test

lint: ## fmt check, clippy, ruff, mypy — everything CI enforces
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	ruff check python/ benchmark/ examples/
	ruff format --check python/ benchmark/ examples/
	mypy python/puffinparse

fmt: ## Format Rust and Python sources in place
	$(CARGO) fmt --all
	ruff format python/ benchmark/ examples/
	ruff check --fix python/ benchmark/ examples/

develop: ## Build the PyO3 extension and install puffinparse into the active venv
	maturin develop

bench: ## Run the benchmark against the default dataset and models
	$(CLI) bench run --dataset $(BENCH_DATASET) --models $(BENCH_MODELS)

dataset: ## Regenerate the synthetic-v1 benchmark dataset
	$(PYTHON) benchmark/generate_synthetic.py

leaderboard: ## Regenerate benchmark/LEADERBOARD.md from committed results
	$(CLI) bench report --by-dataset --intro benchmark/LEADERBOARD.intro.md benchmark/results/*.json > benchmark/LEADERBOARD.md

clean: ## Remove Rust build artifacts
	$(CARGO) clean
