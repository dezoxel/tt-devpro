SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
MAKEFLAGS += --warn-undefined-variables
MAKEFLAGS += --no-builtin-rules

docker_compose := docker compose -f docker-compose.dev.yml
dev_container := tt-devpro-app-1
image_name := tt-devpro
ARGS ?=

.PHONY: help
help:
	@echo "TT DevPro - Available Commands"
	@echo ""
	@echo "Usage mode (pre-built image):"
	@echo "  make build      Build production Docker image"
	@echo "  make tt CMD     Run tt command (auto-detects dev/prod mode)"
	@echo ""
	@echo "Dev mode (hot reload):"
	@echo "  make dev-start  Start dev environment with hot reload"
	@echo "  make dev-stop   Stop dev environment"
	@echo "  make dev-logs   Show container logs"
	@echo ""
	@echo "Other:"
	@echo "  make auth       Refresh DevPro token via browser (runs on host)"
	@echo "  make clean      Remove containers, volumes, and images"
	@echo ""
	@echo "Examples:"
	@echo "  make build && make tt settle      # Usage mode"
	@echo "  make dev-start && make tt settle  # Dev mode"
	@echo "  make tt settle --from 2025-12-01 --to 2025-12-15"


# =============================================================================
# Build (production image)
# =============================================================================
.PHONY: build

build:
	@echo "Building production Docker image..."
	docker build -t $(image_name) .
	@echo "Done. Run 'make tt <command>' to use."


# =============================================================================
# CLI Commands (auto-detects dev/prod mode)
# =============================================================================
.PHONY: tt

# Check if dev container is running
define is_dev_running
$(shell docker ps --filter "name=$(dev_container)" --filter "status=running" -q 2>/dev/null)
endef

# Volume mounts for production mode
define prod_volumes
-v $(HOME)/.tt-token:/root/.tt-token:ro \
-v $(HOME)/.tt-config.yaml:/root/.tt-config.yaml:ro \
-v $(HOME)/knowledge-base:/Users/iurii.buchchenko/knowledge-base:ro
endef

tt:
	@if [ -n "$(ARGS)" ]; then \
		args="$(ARGS)"; \
	else \
		args="$(filter-out $@,$(MAKECMDGOALS))"; \
	fi; \
	if [ -n "$(is_dev_running)" ]; then \
		if [ -t 0 ]; then \
			docker exec -it $(dev_container) gradle run --args="$$args" --quiet; \
		else \
			docker exec -i $(dev_container) gradle run --args="$$args" --quiet; \
		fi; \
	else \
		if ! docker image inspect $(image_name) >/dev/null 2>&1; then \
			echo "Error: No production image found. Run 'make build' first."; \
			exit 1; \
		fi; \
		if [ -t 0 ]; then \
			docker run --rm -it $(prod_volumes) $(image_name) $$args; \
		else \
			docker run --rm -i $(prod_volumes) $(image_name) $$args; \
		fi; \
	fi

# Catch-all target to allow passing arguments to tt
%:
	@:


# =============================================================================
# Dev mode (hot reload)
# =============================================================================
.PHONY: dev-start dev-stop dev-logs

dev-start:
	@echo "Starting dev environment with hot reload..."
	$(docker_compose) up --build -d
	@echo "Container started. Use 'make dev-logs' to view output."

dev-stop:
	@echo "Stopping dev environment..."
	$(docker_compose) down

dev-logs:
	$(docker_compose) logs -f


# =============================================================================
# Auth (runs on host, not in Docker)
# =============================================================================
.PHONY: auth

auth:
	@echo "Refreshing DevPro token via browser..."
	./auth.sh


# =============================================================================
# Cleanup
# =============================================================================
.PHONY: clean

clean:
	@echo "Cleaning up..."
	-$(docker_compose) down -v 2>/dev/null
	-docker rmi $(image_name) 2>/dev/null
	@echo "Done."
