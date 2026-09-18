#!/bin/sh
# One-shot setup for the OFFLINE AI demo: makes Robrix's `/ai` chat work with
# NO API key, using the deterministic `scenario` provider compiled into octos
# (acp-embedding-seam). Run this once in a normal terminal (not a sandbox):
#
#   sh a2app/dev/offline-ai-setup.sh
#
# What it writes:
#   ~/.octos/config.json  <- octos provider config, read by BOTH the embedded
#                            agent and Robrix's setup/Providers-page checks.
#
# The config says two things:
#   - provider: "scenario"            -> octos runs the offline deterministic
#                                        model (host-tool role calls
#                                        `launch_splash_app`, writer role emits
#                                        a canned Splash app).
#   - env_vars.ANTHROPIC_API_KEY      -> a placeholder so Robrix's "is anything
#     (any non-empty value)              configured?" gate passes. Never sent
#                                        anywhere: the scenario provider ignores
#                                        keys.
#
# Existing settings are preserved: if ~/.octos/config.json already exists this
# only fills in the `provider` and `env_vars` fields and leaves everything else
# (a real key, mcp_servers, gateway settings) untouched. To go back to a real
# model later, flip `provider` back to your provider (or delete the file) and
# remove the placeholder key.

set -eu

# octos/Robrix resolve config.json from the first existing candidate
# (OCTOS_CONFIG_DIR is authoritative), so write to whichever will actually be
# read — defaulting to the conventional ~/.octos/config.json.
if [ -n "${OCTOS_CONFIG_DIR:-}" ]; then
    CONFIG="$OCTOS_CONFIG_DIR/config.json"
else
    CONFIG="${XDG_CONFIG_HOME:-$HOME/.config}/octos/config.json"
    [ -f "$CONFIG" ] || CONFIG="$HOME/.octos/config.json"
fi

mkdir -p "$(dirname "$CONFIG")"

# Merge into any existing config rather than clobbering it.
if [ -f "$CONFIG" ]; then
    tmp="$(mktemp)"
    # Rewrite provider + env_vars with jq if present, else fall back to python3.
    if command -v jq >/dev/null 2>&1; then
        jq '.provider = "scenario"
            | .env_vars.ANTHROPIC_API_KEY = "sk-dummy-testing"
            | .version = 1' "$CONFIG" > "$tmp" && mv "$tmp" "$CONFIG"
    else
        python3 - "$CONFIG" <<'PY'
import json, sys
path = sys.argv[1]
with open(path) as f:
    cfg = json.load(f)
cfg["provider"] = "scenario"
cfg.setdefault("env_vars", {})["ANTHROPIC_API_KEY"] = "sk-dummy-testing"
cfg["version"] = 1
with open(path, "w") as f:
    json.dump(cfg, f, indent=2)
    f.write("\n")
PY
    fi
else
    cat > "$CONFIG" <<'EOF'
{
  "version": 1,
  "provider": "scenario",
  "env_vars": {
    "ANTHROPIC_API_KEY": "sk-dummy-testing"
  }
}
EOF
fi

chmod 600 "$CONFIG"
echo "Wrote $CONFIG"
echo
echo "Now run Robrix and use /ai in any room:"
echo "  cargo run --features a2app-embedded-agent"
