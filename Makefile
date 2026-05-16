# Velocity — local dev Makefile
# All commands assume `make` is run from the repo root.

SHELL          := /usr/bin/env bash
.SHELLFLAGS    := -eu -o pipefail -c
.DEFAULT_GOAL  := help

# --- Config ---
COMPOSE        ?= docker compose
PG_HOST        ?= localhost
PG_PORT        ?= 5434
PG_DB          ?= velocity
PG_SUPERUSER   ?= postgres
PG_SUPERPASS   ?= postgres
PG_API_USER    ?= velocity_api
PG_API_PASS    ?= velocity_api_dev

PG_SUPER_URL   := postgres://$(PG_SUPERUSER):$(PG_SUPERPASS)@$(PG_HOST):$(PG_PORT)/$(PG_DB)
PG_API_URL     := postgres://$(PG_API_USER):$(PG_API_PASS)@$(PG_HOST):$(PG_PORT)/$(PG_DB)

# Run psql inside the postgres container so the host doesn't need libpq/psql.
# -T disables TTY (for non-interactive); interactive targets use the form without -T.
PSQL_SUPER     := $(COMPOSE) exec -T -e PGPASSWORD=$(PG_SUPERPASS) postgres psql -U $(PG_SUPERUSER) -d $(PG_DB) -v ON_ERROR_STOP=1
PSQL_SUPER_I   := $(COMPOSE) exec    -e PGPASSWORD=$(PG_SUPERPASS) postgres psql -U $(PG_SUPERUSER) -d $(PG_DB)
PSQL_API_I     := $(COMPOSE) exec    -e PGPASSWORD=$(PG_API_PASS)  postgres psql -U $(PG_API_USER)  -d $(PG_DB)
PSQL_API       := $(COMPOSE) exec -T -e PGPASSWORD=$(PG_API_PASS)  postgres psql -U $(PG_API_USER)  -d $(PG_DB) -v ON_ERROR_STOP=1

DATA_DIRS      := data/postgres data/redis data/kafka data/typesense

# --- Help ---
.PHONY: help
help: ## Show this help
	@awk 'BEGIN {FS = ":.*##"; printf "\nUsage:\n  make \033[36m<target>\033[0m\n\nTargets:\n"} \
	  /^[a-zA-Z_0-9-]+:.*?##/ { printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2 }' $(MAKEFILE_LIST)

# --- Infra lifecycle ---
.PHONY: up
up: data-dirs ## Start all dev infra (postgres, redis, kafka, typesense)
	$(COMPOSE) up -d
	@$(MAKE) --no-print-directory wait-pg
	@echo "→ Postgres ready at $(PG_HOST):$(PG_PORT)"

.PHONY: up-pg
up-pg: data-dirs ## Start only postgres
	$(COMPOSE) up -d postgres
	@$(MAKE) --no-print-directory wait-pg

.PHONY: down
down: ## Stop all dev infra (keeps data volumes)
	$(COMPOSE) down

.PHONY: nuke
nuke: ## Stop infra AND delete all local data (DESTRUCTIVE)
	$(COMPOSE) down -v
	rm -rf $(DATA_DIRS)
	@echo "→ data/ wiped"

.PHONY: restart
restart: down up ## Restart all infra

.PHONY: ps
ps: ## List running infra
	$(COMPOSE) ps

.PHONY: logs
logs: ## Tail logs for all infra
	$(COMPOSE) logs -f --tail=100

.PHONY: logs-pg
logs-pg: ## Tail postgres logs
	$(COMPOSE) logs -f --tail=100 postgres

.PHONY: data-dirs
data-dirs: ## Create local data volume directories
	@mkdir -p $(DATA_DIRS)

.PHONY: wait-pg
wait-pg: ## Block until postgres is accepting connections
	@for i in $$(seq 1 30); do \
	  if $(COMPOSE) exec -T postgres pg_isready -U $(PG_SUPERUSER) -d $(PG_DB) >/dev/null 2>&1; then \
	    exit 0; \
	  fi; \
	  sleep 1; \
	done; \
	echo "postgres failed to become ready"; exit 1

# --- Database (runs psql inside the postgres container — no host psql needed) ---
.PHONY: psql
psql: ## Open psql as superuser (interactive, inside container)
	@$(PSQL_SUPER_I)

.PHONY: psql-api
psql-api: ## Open psql as velocity_api (interactive, verifies NOBYPASSRLS path)
	@$(PSQL_API_I)

.PHONY: db-bootstrap
db-bootstrap: wait-pg ## Re-apply db/init/*.sql against the running cluster (idempotent)
	@for f in db/init/*.sql; do \
	  echo "→ applying $$f"; \
	  $(PSQL_SUPER) < "$$f"; \
	done

.PHONY: db-verify-rls
db-verify-rls: ## Assert velocity_api has NOBYPASSRLS (ADR-007)
	@result=$$($(PSQL_API) -tAc "SELECT rolbypassrls FROM pg_roles WHERE rolname = current_user" | tr -d '[:space:]'); \
	if [ "$$result" = "f" ]; then \
	  echo "✓ velocity_api NOBYPASSRLS"; \
	else \
	  echo "✗ velocity_api has BYPASSRLS=true — fix the role (got '$$result')"; exit 1; \
	fi

.PHONY: migrate
migrate: wait-pg ## Apply platform.* migrations (numeric order)
	@if [ ! -d migrations ] || [ -z "$$(ls -A migrations 2>/dev/null)" ]; then \
	  echo "no migrations yet — skipping"; exit 0; \
	fi; \
	for f in migrations/*.sql; do \
	  echo "→ applying $$f"; \
	  $(PSQL_SUPER) < "$$f"; \
	done

.PHONY: db-reset
db-reset: ## Drop + recreate the velocity database
	@$(COMPOSE) exec -T -e PGPASSWORD=$(PG_SUPERPASS) postgres psql -U $(PG_SUPERUSER) -d postgres -v ON_ERROR_STOP=1 \
	  -c "DROP DATABASE IF EXISTS $(PG_DB) WITH (FORCE); CREATE DATABASE $(PG_DB);"
	@$(MAKE) --no-print-directory db-bootstrap migrate

.PHONY: db-url
db-url: ## Print connection URLs
	@echo "superuser: $(PG_SUPER_URL)"
	@echo "api:       $(PG_API_URL)"

.PHONY: db-smoke
db-smoke: ## Smoke-test platform schema: audit chain + velocity_api gates
	@echo "→ writing two audit rows via the proc"
	@$(PSQL_SUPER) -c "SELECT platform.audit_insert('smoke', 'create', 'success', 'smoke/test/x/y/v1', gen_random_uuid(), '{}'::jsonb, NULL, 'smoke-1', NULL, NULL);" >/dev/null
	@$(PSQL_SUPER) -c "SELECT platform.audit_insert('smoke', 'update', 'success', 'smoke/test/x/y/v1', gen_random_uuid(), '{}'::jsonb, NULL, 'smoke-2', NULL, NULL);" >/dev/null
	@echo "→ verifying chain integrity"
	@tampered=$$($(PSQL_SUPER) -tAc "SELECT count(*) FROM platform.audit_verify_window(now() - interval '1 hour', now() + interval '1 hour') WHERE stored_hash != computed_hash" | tr -d '[:space:]'); \
	  test "$$tampered" = "0" && echo "  ✓ chain intact ($$tampered tampered rows)" || (echo "  ✗ chain has $$tampered tampered rows" && exit 1)
	@echo "→ verifying velocity_api can call audit_insert"
	@$(PSQL_API) -c "SELECT platform.audit_insert('smoke', 'read', 'success', 'smoke/test/x/y/v1', gen_random_uuid(), '{}'::jsonb, NULL, 'smoke-3', NULL, NULL);" >/dev/null && echo "  ✓ velocity_api → audit_insert works"
	@echo "→ verifying velocity_api CANNOT INSERT into audit_log directly (ADR-005)"
	@if $(PSQL_API) -c "INSERT INTO platform.audit_log (occurred_at, actor, action, outcome, hash) VALUES (now(), 'smoke', 'r', 's', 'x')" >/dev/null 2>&1; then \
	  echo "  ✗ direct INSERT succeeded — REVOKE is broken"; exit 1; \
	else echo "  ✓ direct INSERT denied"; fi
	@echo "→ cleaning up smoke rows"
	@$(PSQL_SUPER) -c "DELETE FROM platform.audit_log WHERE actor='smoke';" >/dev/null
	@$(PSQL_SUPER) -c "UPDATE platform.audit_chain_state SET last_hash=(SELECT hash FROM platform.audit_log ORDER BY occurred_at DESC, id DESC LIMIT 1) WHERE id=1;" >/dev/null
	@echo "db-smoke: PASS"

# --- Rust workspace ---
.PHONY: build
build: ## cargo build --workspace
	cargo build --workspace

.PHONY: test
test: ## cargo test --workspace
	cargo test --workspace

.PHONY: fmt
fmt: ## cargo fmt
	cargo fmt --all

.PHONY: fmt-check
fmt-check: ## cargo fmt --check
	cargo fmt --all -- --check

.PHONY: clippy
clippy: ## cargo clippy -D warnings
	cargo clippy --workspace --all-targets -- -D warnings

.PHONY: audit
audit: ## cargo audit
	cargo audit

.PHONY: check
check: fmt-check clippy test ## Full pre-commit check

.PHONY: generate-crds
generate-crds: ## Regenerate crds/*.yaml from velocity-types
	cargo run -p velocity-types --bin generate-crds

# --- Operator ---
.PHONY: operator
operator: ## Run velocity-operator locally against docker-compose postgres
	VELOCITY_OPERATOR_PG_URL=postgres://velocity_operator:velocity_operator_dev@$(PG_HOST):$(PG_PORT)/$(PG_DB) \
	VELOCITY_OPERATOR_PRETTY_LOGS=true \
	RUST_LOG=$${RUST_LOG:-info,velocity_operator=debug} \
	cargo run -p velocity-operator

.PHONY: operator-test
operator-test: ## Run velocity-operator tests (incl. integration vs docker-compose postgres)
	VELOCITY_OPERATOR_PG_URL=postgres://postgres:postgres@$(PG_HOST):$(PG_PORT)/$(PG_DB) \
	cargo test -p velocity-operator

# --- End-to-end ---
.PHONY: e2e
e2e: ## Run Phase 0 end-to-end test against minikube (see tests/e2e/run.sh)
	@bash tests/e2e/run.sh

.PHONY: e2e-clean
e2e-clean: ## Tear down everything `make e2e` created (helm release, namespaces, stray operator)
	-helm -n velocity-system uninstall velocity --ignore-not-found 2>/dev/null
	-kubectl delete ns velocity-system --ignore-not-found --timeout=30s
	-kubectl delete ns acme-supply-chain --ignore-not-found --timeout=30s
	-pkill -f 'target/(debug|release)/velocity-operator' 2>/dev/null || true
	-rm -rf data/webhook-tls

# --- Convenience ---
.PHONY: dev
dev: up-pg db-bootstrap db-verify-rls ## One-shot: bring up pg, bootstrap roles, verify RLS
	@echo ""
	@echo "Velocity dev DB ready."
	@$(MAKE) --no-print-directory db-url
