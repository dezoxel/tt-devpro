SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
MAKEFLAGS += --warn-undefined-variables
MAKEFLAGS += --no-builtin-rules

docker_compose := docker compose -f docker-compose.dev.yml
container := tt-devpro-app-1

.PHONY: help
help:
	@echo "TT DevPro - Available Commands"
	@echo ""
	@echo "Docker:"
	@echo "  make start      Start dev environment with hot reload"
	@echo "  make stop       Stop dev environment"
	@echo "  make restart    Restart dev environment"
	@echo "  make logs       Show container logs"
	@echo "  make clean      Stop and remove volumes (Gradle cache)"
	@echo ""
	@echo "Build:"
	@echo "  make build      Build the application"
	@echo ""
	@echo "CLI (runs inside Docker):"
	@echo "  make tt CMD     Run tt command (e.g., make tt list)"
	@echo ""
	@echo "Auth:"
	@echo "  make auth       Refresh DevPro token via browser (runs on host)"
	@echo ""
	@echo "Examples:"
	@echo "  make tt list"
	@echo "  make tt projects"
	@echo "  make tt fill --from 2025-12-01 --to 2025-12-15"


# =============================================================================
# Docker Lifecycle
# =============================================================================
.PHONY: start stop restart logs clean

start:
	@echo "Starting dev environment with hot reload..."
	$(docker_compose) up --build -d
	@echo "Container started. Use 'make logs' to view output."

stop:
	@echo "Stopping dev environment..."
	$(docker_compose) down

restart:
	@echo "Restarting dev environment..."
	$(docker_compose) restart

logs:
	$(docker_compose) logs -f

clean:
	@echo "Cleaning up dev environment and Gradle cache..."
	$(docker_compose) down -v


# =============================================================================
# Build
# =============================================================================
.PHONY: build

build:
	@echo "Building application..."
	./gradlew build


# =============================================================================
# CLI Commands (via Docker)
# =============================================================================
.PHONY: tt

# Run tt command inside Docker container
# Usage: make tt list, make tt fill --from 2025-12-01 --to 2025-12-15
tt:
	@if [ -t 0 ]; then \
		docker exec -it $(container) gradle run --args="$(filter-out $@,$(MAKECMDGOALS))" --quiet; \
	else \
		docker exec -i $(container) gradle run --args="$(filter-out $@,$(MAKECMDGOALS))" --quiet; \
	fi

# Catch-all target to allow passing arguments to tt
%:
	@:


# =============================================================================
# Auth (runs on host, not in Docker)
# =============================================================================
.PHONY: auth

auth:
	@echo "Refreshing DevPro token via browser..."
	./auth.sh
