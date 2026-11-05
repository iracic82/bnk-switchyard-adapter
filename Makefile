SHELL := /bin/bash
CARGO ?= cargo
KIND_CLUSTER ?= sy-validate

.PHONY: build test lint fmt docker-build demo-local local-up local-down e2e benchmark \
        validate-manifests kind-smoke extproc-test observability-up observability-down clean

build:
	$(CARGO) build --workspace

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) fmt --all --check
	$(CARGO) clippy --workspace --all-targets -- -D warnings
	python3 -m py_compile tests/e2e_local.py bench/run_bench.py bench/workload.py

fmt:
	$(CARGO) fmt --all

docker-build:
	docker build --target adapter -t bnk-switchyard-adapter:dev .
	docker build --target mock-llm -t mock-llm:dev .

local-up: build
	./scripts/local-up.sh

local-down:
	./scripts/local-down.sh

e2e:
	python3 tests/e2e_local.py

# Starts the local stack, runs the narrated demo and the e2e checks, then stops.
# KEEP=1 leaves the stack running.
demo-local: build
	./scripts/local-down.sh
	JUDGE_ERROR_RATE=0 ./scripts/local-up.sh
	./scripts/demo.sh
	python3 tests/e2e_local.py || (./scripts/local-down.sh; exit 1)
	@if [[ -z "$(KEEP)" ]]; then ./scripts/local-down.sh; else echo "stack left running: make local-down"; fi

# ext_proc contract with a real Envoy in Docker, against the local stack.
extproc-test: build
	./scripts/local-down.sh
	JUDGE_ERROR_RATE=0 ./scripts/local-up.sh
	./scripts/extproc-test.sh; status=$$?; ./scripts/local-down.sh; exit $$status

# Mock-backed benchmark. Pass BNK_URL=http://<external VIP> to add the switchyard-bnk scenario.
benchmark: build
	./scripts/local-down.sh
	CONFIG=config/adapter.bench.toml ./scripts/local-up.sh
	python3 bench/run_bench.py --episodes $(or $(EPISODES),60) --concurrency $(or $(CONCURRENCY),16) \
	  $(if $(BNK_URL),--bnk-url $(BNK_URL) --metrics-url $(METRICS_URL),) ; status=$$?; \
	  ./scripts/local-down.sh; exit $$status

# Server-side dry run on kind against Gateway API, GAIE v1.4.0 and (if F5EPP_CRD is set)
# the F5EPP CRD exported from a BNK cluster.
validate-manifests:
	./scripts/validate-manifests.sh

kind-smoke: docker-build
	./scripts/kind-smoke.sh

observability-up:
	docker compose -f observability/docker-compose.yml up -d

observability-down:
	docker compose -f observability/docker-compose.yml down

clean:
	./scripts/local-down.sh
	$(CARGO) clean
