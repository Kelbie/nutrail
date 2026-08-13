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
                                runway  ──melt──▶ BitLaunch hosting invoice
```

The mint is the always-online Lightning node, so you never run one. Built on
[CDK](https://github.com/cashubtc/cdk).

## Spin up (one command, paid in sats)

You need: a [BitLaunch](https://bitlaunch.io) account with an API token
([create one here](https://app.bitlaunch.io/account/api)), any lightning wallet,
and `curl`, `jq`, `openssl` (plus `qrencode` for the terminal QR).

```sh
git clone https://github.com/Kelbie/nutrail && cd nutrail
BL_API_TOKEN=your-token PAYOUT_LN_ADDRESS=you@wallet.com ./deploy/spinup.sh
```

What happens:

1. If your BitLaunch balance is under $10, the script creates a **$20 Lightning
   funding invoice and shows it as a QR in your terminal** (BitLaunch's minimum
   deposit). Pay it with any lightning wallet; the script waits for the credit.
2. **Wallet seed entropy is generated locally** on your machine with
   `openssl rand -hex 32` (OS CSPRNG, 32 bytes → a 24-word BIP-39 mnemonic) and
   passed to the server as the `SEED_ENTROPY_HEX` env var. Secrets are written
   to a local `chmod 600` file.
3. A **$10/mo 1GB BitLaunch VPS** (`nibble-1024`, billed hourly from your
   balance) boots straight into nutrail via Docker, behind Caddy with
   automatic HTTPS on a zero-config `<ip>.sslip.io` domain.
4. It prints your donate page, admin URL, and the embed snippet:

```html
<script src="https://your-server.sslip.io/donate/widget.js" defer></script>
```

**First thing after spin-up: open the admin URL and write the 24 words on
paper.** Then delete the local secrets file.

Knobs (env vars for `spinup.sh`): `MINT_URL` (default
`mint.minibits.cash/Bitcoin` — pick a mint you trust), `REGION` (default
`lon1`), `RESERVE_SATS` (default `30000`), `TOPUP_USD` (default `20`), `SIZE`,
`NAME`, `IMAGE`.

## The self-funding runway

- The sweeper runs every 30s: everything above `RESERVE_SATS` melts to your
  `PAYOUT_LN_ADDRESS`; the reserve stays in the wallet as ecash.
- Every 6 hours the server checks its own BitLaunch balance (`GET /user`).
  When it drops below `BL_MIN_BALANCE_USD` (default $5 ≈ two weeks of runway),
  it creates a `BL_TOPUP_USD` (default $20, the minimum) Lightning funding
  transaction and **pays the invoice by melting its ecash reserve**.
- If the reserve can't cover a top-up, it emits a `runway_underfunded` event
  (visible on `/events` and the admin page) and retries next cycle.
- `GET /runway` (authed) reports wallet sats, reserve, BitLaunch balance, and
  days of hosting left.

Sizing: at $10/mo hosting, a $20 top-up is roughly 60 days of runway;
`RESERVE_SATS=30000` covers a top-up with margin at current prices. Donations
beyond that go straight to your wallet.

## Environment (server)

| Var | Required | Default | Purpose |
|---|---|---|---|
| `SETUP_TOKEN` | yes | — | Bearer token for the wallet API + admin page |
| `SEED_ENTROPY_HEX` | no | — | 16–32 bytes of hex entropy → deterministic BIP-39 mnemonic (what `spinup.sh` passes). Without it, the server generates from its own CSPRNG on first boot |
| `MINT_URL` | no | testnut | The Cashu mint to receive through |
| `PAYOUT_LN_ADDRESS` | no | — | **Any lightning address**; the sweeper melts everything above the reserve to it every 30s. Unset = accumulate as ecash |
| `RESERVE_SATS` | no | `0` | Ecash held back as the hosting runway |
| `BL_API_TOKEN` | no | — | Enables self-funding against your BitLaunch account |
| `BL_MIN_BALANCE_USD` / `BL_TOPUP_USD` | no | `5` / `20` | When to top up, and by how much |
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
| `GET /runway` | — | wallet sats, reserve, BitLaunch balance + days left |
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
- **`BL_API_TOKEN` lives on the server** so it can fund itself. That token can
  also create servers on your account — use a dedicated BitLaunch account
  funded with small amounts, or omit the token and top up manually.
- **Failed melts are recoverable.** Proofs persist in sqlite; the sweeper
  retries (`finalize_pending_melts` + saga recovery on boot); worst case the
  24 words restore unspent proofs on any CDK/Nutshell wallet (NUT-13, per-mint).

## Alternative: Railway

The repo still ships `railway.toml` + a template; see git history for the
Railway walkthrough (`railway init` → volume → variables → `railway up`).
Railway can't be paid in sats, which is why BitLaunch is the primary path.

## Local dev

```sh
SETUP_TOKEN=dev DATA_DIR=./data cargo run
open "http://localhost:3000/donate"
open "http://localhost:3000/admin?token=dev"
```

The default mint is `testnut.cashudevkit.org` — a test mint with worthless
sats that auto-pays invoices, ideal for trying the full pipeline.
