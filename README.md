# nutrail

A donation server that **pays for its own hosting**. Drop one `<script>` tag on
any website (including fully static sites) and receive donations via
**Lightning, Cashu ecash, or on-chain** — surplus auto-melts to your lightning
address, while a small ecash runway stays behind and settles the server's
Bitcoin-paid hosting bill over Lightning when it comes due.

```
donor ──lightning/onchain──▶ Cashu mint ──ecash──▶ nutrail (VPS)
donor ──cashu token/creq───────────────────────▶ nutrail (VPS)
                                                     │
                                surplus ──melt──▶ your lightning address
                                runway  ──melt──▶ LNVPS renewal invoice
```

The mint is the always-online Lightning node, so you never run one. Built on
[CDK](https://github.com/cashubtc/cdk).

## Spin up (paid in sats, ~6-9k sats/month)

You need [bun](https://bun.sh), `ssh-keygen`, and any lightning wallet. No
account signup anywhere — your LNVPS identity is a nostr key derived from the
seedphrase (NIP-06).

```sh
git clone https://github.com/Kelbie/nutrail && cd nutrail/deploy
bun install && bun spinup.ts
```

The CLI walks you through it:

1. **Generates a 12-word seedphrase locally** (OS CSPRNG). Write it down —
   it is the ecash wallet *and* the hosting account identity.
2. **Pick a mint** (arrow keys): `mint.minibits.cash/Bitcoin` (real sats,
   recommended), `testnut.cashudevkit.org` (CDK test mint — fake sats, so the
   server *cannot* pay its own renewals), or any custom mint URL.
3. Enter the **lightning address** donations get swept to, and pick a region
   with live prices (Dublin ~€3.30/mo ≈ 6k sats; London/Quebec ~€5.00/mo ≈
   9k sats — London recommended, Dublin has had most of LNVPS's incidents).
4. LNVPS creates the VM and the CLI shows the **first month's Lightning
   invoice as a QR in your terminal** — pay it from any wallet.
5. The box is bootstrapped over SSH: Docker + Caddy with automatic HTTPS on
   a zero-config `<ip>.sslip.io` domain, running the prebuilt
   `ghcr.io/kelbie/nutrail` image. You get the donate page, admin URL, and
   embed snippet:

```html
<script src="https://your-server.sslip.io/donate/widget.js" defer></script>
```

`bun spinup.ts --preflight` checks API reachability, auth, and prices without
spending anything.

## The self-funding runway

- The sweeper runs every 30s: everything above the reserve melts to your
  `PAYOUT_LN_ADDRESS`; the reserve stays in the wallet as ecash.
- **The reserve is 1.5× one month's hosting** (50% headroom for bitcoin price
  moves), re-derived from the actual invoice at every renewal.
- Every 6 hours the server checks its own VM expiry (NIP-98-signed call to
  the LNVPS API using the key derived from its seed). **Within 3 days of
  expiry it fetches the renewal invoice and pays it by melting the reserve.**
- If the reserve can't cover a renewal, it emits `runway_underfunded` on
  `/events` and retries every cycle.
- `GET /runway` (authed) reports wallet sats, reserve, VM expiry, and days
  left.

## Environment (server)

| Var | Required | Default | Purpose |
|---|---|---|---|
| `SETUP_TOKEN` | yes | — | Bearer token for the wallet API + admin page |
| `SEED_ENTROPY_HEX` | no | — | 16–32 bytes of hex entropy → deterministic BIP-39 mnemonic (what the spinup CLI passes). Without it, the server generates from its own CSPRNG on first boot |
| `MINT_URL` | no | testnut | The Cashu mint to receive through |
| `PAYOUT_LN_ADDRESS` | no | — | **Any lightning address**; the sweeper melts everything above the reserve to it every 30s. Unset = accumulate as ecash |
| `LNVPS_VM_ID` | no | — | Enables self-funding: the VM this server runs on (set by spinup) |
| `RENEWAL_COST_MSATS` | no | — | First month's invoice amount; seeds the reserve at 1.5× (set by spinup) |
| `RESERVE_SATS` | no | `0` | Manual reserve override when not using LNVPS self-funding |
| `LNVPS_API` | no | `api.lnvps.net` | LNVPS API base |
| `SWEEP_THRESHOLD_SATS` | no | `100` | Sweep only when the surplus reaches this |
| `RESTORE_MNEMONIC` | no | — | NUT-13 restore on a fresh volume, then unset it |
| `PUBLIC_URL` | no | derived | External base URL used in the NUT-18 `creq` transport |
| `DATA_DIR` / `PORT` | no | `/data` / `3000` | Storage and bind port |

## API

The wallet API mirrors [cocod](https://github.com/Egge21M/cocod)'s route table
and `{output}` / `{error}` JSON envelope. Wallet routes require
`Authorization: Bearer $SETUP_TOKEN`.

| Route | cocod equivalent | Notes |
|---|---|---|
| `GET /ping` | `/ping` | health check |
| `GET /status` | `/status` | always `unlocked` (headless boot; no init/unlock phase) |
| `GET /balance` | `/balance` | per-mint sats |
| `POST /receive/cashu {token}` | `/receive/cashu` | redeem an ecash token |
| `POST /receive/bolt11 {amount}` | `/receive/bolt11` | mint quote → returns `{request, id}` |
| `POST /send/cashu {amount}` | `/send/cashu` | create an ecash token |
| `POST /send/bolt11 {invoice}` | `/send/bolt11` | melt to an invoice |
| `GET /mints/list`, `POST /mints/info` | `/mints/*` | single mint in this PoC |
| `GET /history` | `/history` | PoC: pending melt quotes only |
| `GET /events` | `/events` | SSE: `received`, `minted`, `swept`, `self_funded`, `runway_underfunded` |
| `GET /runway` | — | wallet sats, reserve, VM expiry + days left |
| `GET /admin`, `GET /admin/mnemonic` | — | seed backup UI |

Public widget surface (CORS `*`, no auth): `GET /donate/config` (capability-
detected methods + reusable NUT-18 `creq`), `POST /donate/quote {method, amount?}`
(bolt11 or NUT-30 onchain), `GET /donate/quote/:id`, `POST /donate/cashu {token}`,
`POST /donate/nut18` (NUT-18 HTTP transport target), `GET /donate`,
`GET /donate/widget.js`, `GET /donate/qr?data=`.

## Trust model — read this

- **The mint custodies value between receive and melt.** Auto-melt keeps the
  surplus window to ~30 seconds; the runway reserve is standing mint exposure
  (~$20-30). Pick a reputable mint and keep the reserve small.
- **The VPS host can read the box** (env vars, volume). The seed protects the
  float and the runway, not savings. Same trade as any hosted hot wallet.
- **The server holds its own hosting identity** (nostr key from its seed).
  Anyone with the seed controls the float, the runway, and the VM account —
  one more reason the words belong on paper and the balance stays small.
- **LNVPS is a young, small operation** (~92% lifetime uptime, solid recent
  record). The design degrades gracefully: if the box dies, redeploy with the
  same seedphrase and restore.
- **Failed melts are recoverable.** Proofs persist in sqlite; the sweeper
  retries (`finalize_pending_melts` + saga recovery on boot); worst case the
  seed words restore unspent proofs on any CDK/Nutshell wallet (NUT-13, per-mint).

## Alternative: Railway

The repo still ships `railway.toml` + a template; see git history for the
Railway walkthrough (`railway init` → volume → variables → `railway up`).
Railway can't be paid in sats, which is why LNVPS is the primary path.

## Local dev

```sh
SETUP_TOKEN=dev DATA_DIR=./data cargo run
open "http://localhost:3000/donate"
open "http://localhost:3000/admin?token=dev"
```

The default mint is `testnut.cashudevkit.org` — a test mint with worthless
sats that auto-pays invoices, ideal for trying the full pipeline.
