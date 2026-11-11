#!/usr/bin/env bash
# Exercise binary installers in an isolated filesystem with release fixtures.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/tools" "$WORK/fixtures/codex/bin" "$WORK/fixtures/codex/codex-path" "$WORK/fixtures/codex/codex-resources" "$WORK/fixtures/opencode"
cat > "$WORK/fixtures/opencode/opencode" <<'BINARY'
#!/bin/sh
printf '<%s>\n' "$@"
BINARY
cp "$WORK/fixtures/opencode/opencode" "$WORK/fixtures/codex/bin/codex"
cp "$WORK/fixtures/opencode/opencode" "$WORK/fixtures/codex/bin/codex-code-mode-host"
chmod +x "$WORK/fixtures/"{opencode/opencode,codex/bin/*}
printf '%s\n' '{"layoutVersion":1,"entrypoint":"bin/codex"}' > "$WORK/fixtures/codex/codex-package.json"
touch "$WORK/fixtures/codex/codex-path/rg" "$WORK/fixtures/codex/codex-resources/bwrap"
tar -czf "$WORK/codex.tar.gz" -C "$WORK/fixtures/codex" .
tar -czf "$WORK/opencode.tar.gz" -C "$WORK/fixtures/opencode" .
cat > "$WORK/tools/curl" <<'CURL'
#!/bin/sh
[ "$1" = -fsSL ] && [ "$3" = -o ] || exit 8
printf '%s\n' "$2" >> "$WORK/urls"
case "$2" in
  *codex-package-${EXPECTED_CODEX}-unknown-linux-musl.tar.gz) cp "$WORK/codex.tar.gz" "$4" ;;
  *opencode-linux-${EXPECTED_OPENCODE}.tar.gz) cp "$WORK/opencode.tar.gz" "$4" ;;
  *) exit 9 ;;
esac
CURL
cat > "$WORK/tools/uname" <<'UNAME'
#!/bin/sh
echo "$TEST_ARCH"
UNAME
chmod +x "$WORK/tools/"*
export WORK
for TEST_ARCH in x86_64 aarch64; do
  export TEST_ARCH
  case "$TEST_ARCH" in
    x86_64) EXPECTED_CODEX=x86_64; EXPECTED_OPENCODE=x64-baseline ;;
    aarch64) EXPECTED_CODEX=aarch64; EXPECTED_OPENCODE=arm64 ;;
  esac
  export EXPECTED_CODEX EXPECTED_OPENCODE
  for agent in codex opencode; do
    mkdir -p "$WORK/root/usr/local/bin"
    echo stale-shim > "$WORK/root/usr/local/bin/$agent"
    if [[ "$agent" == codex ]]; then
      mkdir -p "$WORK/root/usr/codex-path" "$WORK/root/usr/codex-resources"
      echo legacy > "$WORK/root/usr/codex-package.json"
    fi
    sed "s|/usr/|$WORK/root/usr/|g" "$ROOT/templates/install-$agent.sh" > "$WORK/install.sh"
    PATH="$WORK/tools:$PATH" sh "$WORK/install.sh"
    [ ! -e "$WORK/root/usr/local/bin/$agent" ]
    if [[ "$agent" == codex ]]; then
      [ "$(readlink "$WORK/root/usr/bin/codex")" = "$WORK/root/usr/lib/codex/bin/codex" ]
    else
      [ ! -L "$WORK/root/usr/bin/$agent" ]
    fi
    [ "$("$WORK/root/usr/bin/$agent" 'one two' --version)" = $'<one two>\n<--version>' ]
  done
  [ ! -e "$WORK/root/usr/codex-package.json" ]
  [ ! -e "$WORK/root/usr/codex-path" ]
  [ ! -e "$WORK/root/usr/codex-resources" ]
  cmp "$WORK/fixtures/codex/codex-package.json" "$WORK/root/usr/lib/codex/codex-package.json"
  [ -x "$WORK/root/usr/bin/codex-code-mode-host" ]
  [ -f "$WORK/root/usr/lib/codex/codex-path/rg" ]
  [ -f "$WORK/root/usr/lib/codex/codex-resources/bwrap" ]
done
[ "$(wc -l < "$WORK/urls")" -eq 4 ]
echo 'PASS: binary installation, release selection, resources, and shim removal'
