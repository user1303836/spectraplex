#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/.."
umask 077
container_uid="$(id -u)"
[[ "$container_uid" != 0 ]] || container_uid=1000
if [[ ! -f .env ]]; then
  command -v openssl >/dev/null || { echo 'openssl is required to generate a secure API key.' >&2; exit 1; }
  key="$(openssl rand -hex 32)"
  printf 'SPECTRAPLEX_API_KEY=%s\nDATABASE_URL=postgres://spectraplex:spectraplex@localhost:5432/spectraplex\n' "$key" > .env
  echo 'Created .env with a random admin key (keep this file private).'
else
  echo 'Keeping existing .env.'
fi
if ! grep -q '^CONTAINER_UID=' .env; then
  printf '\nCONTAINER_UID=%s\n' "$container_uid" >> .env
fi
if [[ ! -f spectraplex.toml ]]; then
  cp spectraplex.toml.example spectraplex.toml
  if [[ "$(id -u)" == 0 ]]; then chown "$container_uid" spectraplex.toml; fi
fi
echo 'Run: docker compose up --build -d'
echo 'Then open http://localhost:3000 and connect with SPECTRAPLEX_API_KEY from .env.'
