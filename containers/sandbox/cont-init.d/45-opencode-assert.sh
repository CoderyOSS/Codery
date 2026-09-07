#!/command/with-contenv bash
# Fail loudly at boot if the opencode binary is missing or unexecutable.
# The Sep 2026 incident had a supervisor silently spinning on exec failures
# for 15 hours — make a broken install visible immediately instead.
if ! command -v opencode >/dev/null 2>&1; then
    echo "[sandbox] ERROR: 'opencode' not found on PATH='${PATH}' — the opencode service will fail to start"
    exit 1
fi
target="$(readlink -f "$(command -v opencode)")"
if [ ! -x "$target" ]; then
    echo "[sandbox] ERROR: opencode target '$target' is not executable — the opencode service will fail to start"
    exit 1
fi
echo "[sandbox] opencode: $(command -v opencode) -> $target"
