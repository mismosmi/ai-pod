#!/usr/bin/env bash
# Regression: the official installer's version probe must not reinstall recursively.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$WORK/bin" "$WORK/home"
sed -n "/^cat > .* <<'SHIM'$/,/^SHIM$/p" "$ROOT/templates/install-opencode.sh" | sed '1d;$d' > "$WORK/bin/opencode"
chmod +x "$WORK/bin/opencode"
cat > "$WORK/bin/curl" <<'CURL'
#!/bin/sh
[ "$*" = '-fsSL https://opencode.ai/v2/install' ] || exit 8
# Bound recursion so a broken shim fails quickly instead of hanging.
[ ! -e "$HOME/downloaded" ] || exit 9
touch "$HOME/downloaded"
cat <<'INSTALL'
[ "$*" = '--no-modify-path' ] || exit 10
# Match the official installer's check_version behavior.
installed_version=$(opencode --version 2>/dev/null || echo "")
[ -z "$installed_version" ] || exit 11
mkdir -p "$HOME/.opencode/bin"
cat > "$HOME/.opencode/bin/opencode" <<'BINARY'
#!/bin/sh
[ -z "${AI_POD_OPENCODE_INSTALLING:-}" ] || exit 12
printf '<%s>\n' "$@"
exit 7
BINARY
chmod +x "$HOME/.opencode/bin/opencode"
INSTALL
CURL
chmod +x "$WORK/bin/curl"
for attempt in 1 2; do
  status=0
  output=$(HOME="$WORK/home" PATH="$WORK/bin:$PATH" "$WORK/bin/opencode" 'one two' --version) || status=$?
  [ "$status" = 7 ]
  [ "$output" = $'<one two>\n<--version>' ]
done
echo 'PASS: installer version probe, cached reuse, arguments, and exit status'
