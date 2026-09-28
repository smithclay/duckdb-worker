#!/usr/bin/env bash
# Deploy to a throwaway Cloudflare account: no signup or login, live for 60 minutes unless
# claimed via the printed claim URL. Uses an empty wrangler config dir so an existing
# wrangler login on this machine is left untouched (--temporary refuses when logged in).
#
#   scripts/deploy-temporary.sh                # prebuilt dist/ (needs only node)
#   scripts/deploy-temporary.sh --from-source  # worker-build (needs tiny/build/full-sb-keep, Rust beta)
set -euo pipefail
cd "$(dirname "$0")/.."
CONFIG=wrangler.prebuilt.toml
[ "${1:-}" = "--from-source" ] && CONFIG=wrangler.toml
ANON_HOME="${ANON_HOME:-$(mktemp -d)}"
echo "wrangler config dir for the temporary account: $ANON_HOME"
env -u CLOUDFLARE_API_TOKEN -u CLOUDFLARE_ACCOUNT_ID XDG_CONFIG_HOME="$ANON_HOME" \
  npx -y wrangler@latest deploy --temporary -c "$CONFIG"
echo
echo "Tail logs:  XDG_CONFIG_HOME=$ANON_HOME npx wrangler tail --temporary -c $CONFIG"
