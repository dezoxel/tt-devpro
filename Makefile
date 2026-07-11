SHELL := bash
.SHELLFLAGS := -eu -o pipefail -c
MAKEFLAGS += --no-builtin-rules

# Point Gradle at a GraalVM JDK (bundles native-image) if one is installed via
# SDKMAN; otherwise Gradle uses its default JDK.
GRAALVM := $(shell ls -d $(HOME)/.sdkman/candidates/java/*graal* 2>/dev/null | sort -V | tail -1)
ifneq ($(GRAALVM),)
export JAVA_HOME := $(GRAALVM)
endif

.PHONY: help build install auth test clean

help:
	@echo "tt-devpro — Available Commands"
	@echo ""
	@echo "  make install   Build and install tt-devpro to ~/.local/bin (native, JVM fallback)"
	@echo "  make build     Build the native image (build/native/nativeCompile/tt-devpro)"
	@echo "  make test      Run the test suite"
	@echo "  make auth      Refresh the Dev.Pro session cookie via browser (runs on host)"
	@echo "  make clean     Remove build artifacts"
	@echo ""
	@echo "After install:  tt-devpro settle --dry-run"

build:
	./gradlew --no-daemon nativeCompile

install:
	./install.sh

auth:
	./auth.sh

test:
	./gradlew --no-daemon test

clean:
	./gradlew --no-daemon clean
