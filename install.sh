#!/usr/bin/env bash
# install.sh — build tt-devpro and install it as a global binary on ~/.local/bin.
#
#   Primary : GraalVM native-image — one self-contained binary, no JVM at runtime.
#   Fallback: gradle installDist    — a JVM launcher (needs a host JDK >= 17).
#
# Native build is used automatically when a GraalVM JDK (with native-image) is
# available; otherwise, or if the native build fails, it falls back to the JVM
# distribution. Force the JVM path with:  ./install.sh --jvm
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

BIN_DIR="$HOME/.local/bin"
LIB_DIR="$HOME/.local/lib/tt-devpro"
mkdir -p "$BIN_DIR"

# --- Resolve a GraalVM JDK (bundles native-image) into JAVA_HOME for the build.
# Gradle, Kotlin compile, installDist and nativeCompile then all share one JDK.
if [ ! -x "${JAVA_HOME:-/nonexistent}/bin/native-image" ]; then
    if [ -d "$HOME/.sdkman/candidates/java" ]; then
        graal="$(ls -d "$HOME/.sdkman/candidates/java"/*graal* 2>/dev/null | sort -V | tail -1 || true)"
        [ -n "$graal" ] && export JAVA_HOME="$graal"
    fi
fi
echo "Building with JAVA_HOME=${JAVA_HOME:-<gradle default>}"

FORCE_JVM="${1:-}"

build_native() {
    [ -x "${JAVA_HOME:-/nonexistent}/bin/native-image" ] || return 1
    echo "Building native image (this can take a few minutes)..."
    ./gradlew --no-daemon nativeCompile
    local bin="build/native/nativeCompile/tt-devpro"
    [ -x "$bin" ] || return 1
    install -m 0755 "$bin" "$BIN_DIR/tt-devpro"
    rm -rf "$LIB_DIR"   # drop any prior JVM-fallback install
    echo "Installed native binary → $BIN_DIR/tt-devpro"
}

build_jvm() {
    echo "Building JVM distribution (installDist)..."
    ./gradlew --no-daemon installDist
    rm -rf "$LIB_DIR"
    mkdir -p "$(dirname "$LIB_DIR")"
    cp -R build/install/tt-devpro "$LIB_DIR"
    # Wrapper pins the runtime JDK to the build JDK if it's still around, so the
    # launcher never picks up an incompatible host java; falls back to PATH java.
    cat > "$BIN_DIR/tt-devpro" <<EOF
#!/usr/bin/env bash
[ -x "${JAVA_HOME:-}/bin/java" ] && export JAVA_HOME="${JAVA_HOME:-}"
exec "$LIB_DIR/bin/tt-devpro" "\$@"
EOF
    chmod +x "$BIN_DIR/tt-devpro"
    echo "Installed JVM launcher → $BIN_DIR/tt-devpro (via $LIB_DIR)"
}

if [ "$FORCE_JVM" = "--jvm" ]; then
    build_jvm
elif build_native; then
    :
else
    echo "Native build unavailable/failed — falling back to JVM installDist."
    build_jvm
fi

echo
case ":$PATH:" in
    *":$BIN_DIR:"*) echo "Done. Run: tt-devpro settle --dry-run" ;;
    *) echo "Done, but $BIN_DIR is not on your PATH. Add it, then run: tt-devpro settle --dry-run" ;;
esac
