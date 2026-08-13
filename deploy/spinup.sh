#!/usr/bin/env bash
# nutrail spin-up: one command from zero to a bitcoin-paid donation server.
#
#   BL_API_TOKEN=... PAYOUT_LN_ADDRESS=you@wallet.com ./deploy/spinup.sh
#
# What it does:
#   1. checks your BitLaunch balance; if low, creates a $20 Lightning funding
#      invoice and shows it as a QR right here in the terminal
#   2. generates the wallet seed entropy locally from your OS CSPRNG
#   3. provisions a $10/mo 1GB BitLaunch VPS that boots straight into nutrail
#      (Docker + Caddy with automatic HTTPS on a sslip.io domain)
#   4. leaves the server holding an ecash runway so it pays its own hosting
#      bill over Lightning when the balance runs low
set -euo pipefail

API="https://app.bitlaunch.io/api"
SIZE="${SIZE:-nibble-1024}"          # 1 vCPU / 1GB / 25GB — $10/mo
REGION="${REGION:-lon1}"
NAME="${NAME:-nutrail}"
MINT_URL="${MINT_URL:-https://mint.minibits.cash/Bitcoin}"
RESERVE_SATS="${RESERVE_SATS:-30000}"        # ecash held back as hosting runway
SWEEP_THRESHOLD_SATS="${SWEEP_THRESHOLD_SATS:-100}"
IMAGE="${IMAGE:-ghcr.io/kelbie/nutrail:latest}"
MIN_BALANCE_MILS=10000                        # provision only with >= $10 credit
TOPUP_USD="${TOPUP_USD:-20}"                  # BitLaunch minimum deposit

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }
need() { command -v "$1" >/dev/null || die "missing dependency: $1"; }

need curl; need jq; need openssl
[ -n "${BL_API_TOKEN:-}" ]       || die "BL_API_TOKEN is required (create one at https://app.bitlaunch.io/account/api)"
[ -n "${PAYOUT_LN_ADDRESS:-}" ]  || die "PAYOUT_LN_ADDRESS is required (any lightning address donations get swept to)"

bl() { # bl METHOD PATH [JSON_BODY]
  local method="$1" path="$2" body="${3:-}"
  curl -sS -X "$method" "$API$path" \
    -H "Authorization: Bearer $BL_API_TOKEN" \
    -H "Content-Type: application/json" \
    ${body:+--data "$body"}
}

balance_mils() { bl GET /user | jq -r '.balance // 0'; }

# ---------------------------------------------------------------- 1. funding
say "▸ checking BitLaunch balance"
BAL=$(balance_mils)
awk -v b="$BAL" 'BEGIN { printf "  balance: $%.2f\n", b/1000 }' 

if [ "$BAL" -lt "$MIN_BALANCE_MILS" ]; then
  say "▸ balance below \$$((MIN_BALANCE_MILS / 1000)) — creating a \$$TOPUP_USD Lightning invoice"
  TX=$(bl POST /transactions "{\"amountUsd\": $TOPUP_USD, \"cryptoSymbol\": \"BTC\", \"lightningNetwork\": true}")
  INVOICE=$(echo "$TX" | jq -r '.address // empty')
  STATUS_URL=$(echo "$TX" | jq -r '.statusUrl // empty')
  case "$INVOICE" in
    ln*|LN*) : ;;
    *) die "did not get a lightning invoice back (got: ${INVOICE:0:24}…). Pay in a browser instead: $STATUS_URL" ;;
  esac

  echo
  if command -v qrencode >/dev/null; then
    qrencode -t ANSIUTF8 -m 2 "${INVOICE}"
  else
    echo "  (install qrencode for a terminal QR)"
  fi
  echo
  echo "  invoice:  $INVOICE"
  echo "  browser:  $STATUS_URL"
  echo
  say "▸ pay that with any lightning wallet — waiting for the balance to update"

  DEADLINE=$(( $(date +%s) + 1200 ))
  while :; do
    sleep 10
    NOW=$(balance_mils)
    if [ "$NOW" -gt "$BAL" ]; then
      awk -v b="$NOW" 'BEGIN { printf "  paid — balance is now $%.2f\n", b/1000 }' 
      break
    fi
    [ "$(date +%s)" -lt "$DEADLINE" ] || die "timed out waiting for payment (invoice may have expired)"
  done
fi

# ------------------------------------------------------------- 2. secrets
say "▸ generating secrets locally (OS CSPRNG)"
SEED_ENTROPY_HEX=$(openssl rand -hex 32)     # 32 bytes → 24-word BIP-39 mnemonic
SETUP_TOKEN=$(openssl rand -hex 16)
ROOT_PASSWORD=$(openssl rand -base64 18 | tr -d '/+=')

SECRETS_FILE="./nutrail-secrets-$(date +%Y%m%d-%H%M%S).txt"
umask 077
cat > "$SECRETS_FILE" <<EOF
nutrail server secrets — keep offline, then delete once the mnemonic is on paper
SEED_ENTROPY_HEX=$SEED_ENTROPY_HEX
SETUP_TOKEN=$SETUP_TOKEN
ROOT_PASSWORD=$ROOT_PASSWORD
EOF
echo "  written to $SECRETS_FILE (chmod 600)"

# ------------------------------------------------------- 3. image + region
say "▸ resolving Ubuntu image for the BitLaunch host"
OPTS=$(bl GET /hosts-create-options/4)
IMAGE_ID=$(echo "$OPTS" | jq -r '[.hostOptions.images[]? // .images[]? | select((.name // "") | test("Ubuntu"; "i"))] | first | (.versions[0].id // .id) // empty')
[ -n "$IMAGE_ID" ] || die "could not find an Ubuntu image in /hosts-create-options/4 — inspect: echo '$OPTS' | jq"
echo "  image id: $IMAGE_ID"

# ------------------------------------------------------------ 4. initscript
INITSCRIPT=$(cat <<CLOUDINIT
#!/bin/bash
set -e
curl -fsSL https://get.docker.com | sh
IP=\$(curl -4 -fsS https://ifconfig.me || hostname -I | awk '{print \$1}')
DOMAIN="\$(echo "\$IP" | tr . -).sslip.io"
mkdir -p /opt/nutrail && cd /opt/nutrail
cat > .env <<ENV
SETUP_TOKEN=$SETUP_TOKEN
SEED_ENTROPY_HEX=$SEED_ENTROPY_HEX
PAYOUT_LN_ADDRESS=$PAYOUT_LN_ADDRESS
MINT_URL=$MINT_URL
RESERVE_SATS=$RESERVE_SATS
SWEEP_THRESHOLD_SATS=$SWEEP_THRESHOLD_SATS
BL_API_TOKEN=$BL_API_TOKEN
PUBLIC_URL=https://\$DOMAIN
DATA_DIR=/data
ENV
cat > Caddyfile <<CADDY
\$DOMAIN {
    reverse_proxy nutrail:3000
}
CADDY
cat > docker-compose.yml <<'COMPOSE'
services:
  nutrail:
    image: $IMAGE
    restart: unless-stopped
    env_file: .env
    volumes: [ "nutrail_data:/data" ]
  caddy:
    image: caddy:2-alpine
    restart: unless-stopped
    ports: [ "80:80", "443:443" ]
    volumes:
      - ./Caddyfile:/etc/caddy/Caddyfile:ro
      - caddy_data:/data
volumes:
  nutrail_data:
  caddy_data:
COMPOSE
docker compose up -d
CLOUDINIT
)

# ------------------------------------------------------------ 5. provision
say "▸ creating $SIZE server in $REGION (\$10/mo, billed hourly from your balance)"
CREATE=$(jq -n \
  --arg name "$NAME" --arg image "$IMAGE_ID" --arg size "$SIZE" \
  --arg region "$REGION" --arg pw "$ROOT_PASSWORD" --arg init "$INITSCRIPT" \
  '{server: {name: $name, hostID: 4, hostImageID: $image, sizeID: $size,
             regionID: $region, password: $pw, initscript: $init}}')
SERVER=$(bl POST /servers "$CREATE")
SERVER_ID=$(echo "$SERVER" | jq -r '.id // .server.id // empty')
[ -n "$SERVER_ID" ] || die "server create failed: $SERVER"
echo "  server id: $SERVER_ID"

say "▸ waiting for the VPS to come up"
for _ in $(seq 1 60); do
  sleep 10
  S=$(bl GET "/servers/$SERVER_ID")
  ST=$(echo "$S" | jq -r '.server.status // .status // ""')
  IP=$(echo "$S" | jq -r '.server.ipv4 // .ipv4 // ""')
  [ "$ST" = "error" ] && die "provisioning failed: $(echo "$S" | jq -r '.server.errorText // .errorText // "unknown"')"
  if [ "$ST" = "ok" ] && [ -n "$IP" ]; then break; fi
done
[ -n "${IP:-}" ] || die "timed out waiting for the server"
DOMAIN="$(echo "$IP" | tr . -).sslip.io"
echo "  up: $IP → https://$DOMAIN"

say "▸ waiting for nutrail + TLS (docker pull and cert issuance take a few minutes)"
for _ in $(seq 1 90); do
  sleep 10
  if curl -fsS -m 5 "https://$DOMAIN/ping" >/dev/null 2>&1; then READY=1; break; fi
done
[ -n "${READY:-}" ] || echo "  not responding yet — give it a few more minutes, then check https://$DOMAIN/ping"

# ------------------------------------------------------------- 6. summary
echo
say "✔ done"
cat <<EOF

  donate page   https://$DOMAIN/donate
  admin         https://$DOMAIN/admin?token=$SETUP_TOKEN
  embed         <script src="https://$DOMAIN/donate/widget.js" defer></script>

  NOW: open the admin page and write the 24 words on paper.
  Then delete $SECRETS_FILE (the entropy in it derives the same seed).

  The server sweeps donations to $PAYOUT_LN_ADDRESS every 30s, keeps
  $RESERVE_SATS sats back as ecash, and when the BitLaunch balance drops
  under \$5 it pays its own \$$TOPUP_USD hosting top-up over Lightning.

  ssh root@$IP  (password in $SECRETS_FILE)
EOF
