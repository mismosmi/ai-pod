#!/bin/sh
# Installed in-container by the ai-pod Dockerfile via:
#   curl http://${HOST_GATEWAY}:7822/install/clipboard.sh | bash
#
# Writes fake `xclip` and `wl-paste` shims that forward clipboard-image reads to
# the host's GET /clipboard/image endpoint. This lets Claude Code's native
# Ctrl+V paste work inside the container, where no real clipboard tools or
# display server exist. The shims read the runtime env vars (AI_POD_SERVER_URL,
# AI_POD_API_KEY, AI_POD_PROJECT_ID) injected into every container, and `curl`
# (present in every base image). /usr/local/bin is ahead of /usr/bin in PATH and
# no real xclip/wl-paste is installed, so there is no conflict.
set -e

# xclip shim. Claude detects available types with `-t TARGETS -o` (greps stdout
# for image/<fmt>) then extracts with `-t image/png -o`. We answer both: print
# the single line `image/png` for TARGETS (only when the host clipboard has an
# image), and stream the PNG bytes otherwise. `curl -fsS` exits non-zero on the
# endpoint's 204 (empty clipboard), so TARGETS prints nothing and Claude
# correctly reports "no image" with no hang.
cat > /usr/local/bin/xclip <<'SHIM'
#!/bin/sh
url="${AI_POD_SERVER_URL}/clipboard/image?project_id=${AI_POD_PROJECT_ID}"
fetch() { curl -fsS -H "X-Api-Key: ${AI_POD_API_KEY}" "$url"; }
case " $* " in
  *" TARGETS "*) if fetch >/dev/null 2>&1; then echo image/png; fi; exit 0 ;;
esac
fetch; exit 0
SHIM
chmod 0755 /usr/local/bin/xclip
echo "Installed xclip shim at /usr/local/bin/xclip"

# wl-paste shim. Same contract, but Wayland lists types with `-l` /
# `--list-types` and extracts with `--type image/png`.
cat > /usr/local/bin/wl-paste <<'SHIM'
#!/bin/sh
url="${AI_POD_SERVER_URL}/clipboard/image?project_id=${AI_POD_PROJECT_ID}"
fetch() { curl -fsS -H "X-Api-Key: ${AI_POD_API_KEY}" "$url"; }
case " $* " in
  *" -l "*|*" --list-types "*) if fetch >/dev/null 2>&1; then echo image/png; fi; exit 0 ;;
esac
fetch; exit 0
SHIM
chmod 0755 /usr/local/bin/wl-paste
echo "Installed wl-paste shim at /usr/local/bin/wl-paste"
