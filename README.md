# nutrail

Minimal [CDK](https://github.com/cashubtc/cdk)-based donation wallet, built to deploy
as a one-click Railway template. Drop one `<script>` tag on any website (including
fully static sites) and receive donations via **Lightning** or **Cashu ecash** —
funds are **auto-melted** in the background to your self-custodial lightning address,
so the server never holds a meaningful balance.

```
donor ──bolt11──▶ Cashu mint ──ecash──▶ nutrail (Railway) ──melt──▶ your LN wallet
donor ──cashu token──────────────────▶ nutrail (Railway) ──melt──▶ your LN wallet
```

The mint is the always-online Lightning node, so you never run one. The wallet seed
is generated **on first boot from the OS CSPRNG** inside the container — never by
the platform, never derived from anything else — and shown once on the admin page
for paper backup.

## Deploy

```sh
railway login
railway init --name my-donations
railway volume add --mount-path /data
railway variables \
  --set "SETUP_TOKEN=$(openssl rand -hex 16)" \
  --set "MINT_URL=https://testnut.cashudevkit.org" \
  --set "PAYOUT_LN_ADDRESS=you@your-ln-provider.com"
railway up
railway domain
```

Then:

1. Open `https://<your-domain>/admin?token=<SETUP_TOKEN>` and **write down the 12 words**.
2. Embed on any site:

```html
<script src="https://<your-domain>/donate/widget.js" defer></script>
<!-- or inline instead of a floating button: -->
<script src="https://<your-domain>/donate/widget.js" data-mode="inline" defer></script>
```

> `testnut.cashudevkit.org` is a **test mint with worthless sats** that auto-pays
> invoices — perfect for trying the pipeline. For real money, pin a reputable mint
> and keep `SWEEP_THRESHOLD_SATS` low.

## Environment

| Var | Required | Default | Purpose |
|---|---|---|---|
| `SETUP_TOKEN` | yes | — | Bearer token for the wallet API + admin page |
| `MINT_URL` | no | testnut | The Cashu mint to receive through |
| `PAYOUT_LN_ADDRESS` | no | — | **Any lightning address** (`you@walletofsatoshi.com`, `you@getalby.com`, Zeus, phoenixd, …). The sweeper auto-melts the whole balance to it every 30s once it clears `SWEEP_THRESHOLD_SATS`. Unset = accumulate as ecash |
| `PUBLIC_URL` | no | Railway domain | External base URL used in the NUT-18 `creq` transport; auto-derived from `RAILWAY_PUBLIC_DOMAIN` or the request Host |
| `SWEEP_THRESHOLD_SATS` | no | `100` | Sweep when balance reaches this |
| `RESTORE_MNEMONIC` | no | — | NUT-13 restore on a fresh volume, then unset it |
| `DATA_DIR` | no | `/data` | Volume mount (sqlite DB + mnemonic) |
| `PORT` | no | `3000` | Set by Railway automatically |

## API

The wallet API mirrors [cocod](https://github.com/Egge21M/cocod)'s route table and
`{output}` / `{error}` JSON envelope. Wallet routes require
`Authorization: Bearer $SETUP_TOKEN`; `/donate/*` routes are public (CORS `*`).

| Route | cocod equivalent | Notes |
|---|---|---|
| `GET /ping` | `/ping` | health check |
| `GET /status` | `/status` | always `unlocked` (headless boot; no init/unlock phase) |
| `GET /balance` | `/balance` | per-mint sats |
| `POST /receive/cashu {token}` | `/receive/cashu` | redeem an ecash token |
| `POST /receive/bolt11 {amount}` | `/receive/bolt11` | mint quote → returns `{request, id}` |
| `POST /send/cashu {amount}` | `/send/cashu` | create an ecash token |
| `POST /send/bolt11 {invoice}` | `/send/bolt11` | melt to an invoice |
| `GET /mints/list` | `/mints/list` | single mint in this PoC |
| `POST /mints/info` | `/mints/info` | NUT-06 info for the configured mint |
| `GET /history` | `/history` | PoC: pending melt quotes only |
| `GET /events` | `/events` | SSE: `received`, `minted`, `swept`, `melt_finalized` |
| `GET /admin`, `GET /admin/mnemonic` | — | nutrail addition (seed backup UI) |

Public widget surface (CORS `*`, no auth):

| Route | Purpose |
|---|---|
| `GET /donate/config` | methods the mint supports (`lightning`, `cashu`, `onchain`), mint URL, reusable NUT-18 `creq` |
| `POST /donate/quote {method, amount?}` | dispatches to a bolt11 (NUT-04) or onchain (NUT-30) mint quote |
| `GET /donate/quote/:id` | quote state (`UNPAID` / `PAID` / `ISSUED`) — same shape for both methods |
| `POST /donate/cashu {token}` | redeem a pasted ecash token |
| `POST /donate/nut18` | NUT-18 HTTP transport target: donor wallets POST the payment payload here after paying the `creq` |
| `GET /donate`, `GET /donate/widget.js`, `GET /donate/qr?data=` | widget page, embed script, QR renderer |

The widget only shows methods the configured mint actually supports (capability
detection at boot from the mint's NUT-04 method list).

Not mirrored (yet): `/init` + `/unlock` (nutrail boots headless and self-initializes;
passphrase-encrypted seed-at-rest is a good follow-up), `/npc/*` (npub.cash),
`/x-cashu/*` (sending side), `/mints/add` (single-mint PoC).

## Trust model — read this

- **The mint custodies value between receive and melt.** Auto-melt keeps that window
  to ~30 seconds and the standing balance under `SWEEP_THRESHOLD_SATS`.
- **Railway can read your volume and env vars.** The seed protects the float, not
  savings. That's the design: this is a hot funnel, not a wallet you store money in.
- **Failed melts are recoverable.** Proofs persist in sqlite; the sweeper retries
  every 30s (`finalize_pending_melts` + saga recovery on boot); worst case, the
  12 words restore unspent proofs on any CDK/Nutshell wallet (NUT-13, per-mint).

## Publishing as a Railway template

The Railway CLI can't publish templates; once your deployment works, open the
project dashboard → **Settings → Publish as template** (volume, Dockerfile build
and variables carry over; mark `SETUP_TOKEN` as a generated secret in the template
composer so each deployer gets a fresh one).

## Local dev

```sh
SETUP_TOKEN=dev DATA_DIR=./data cargo run
open "http://localhost:3000/donate"
open "http://localhost:3000/admin?token=dev"
```
