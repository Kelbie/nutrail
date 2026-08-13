// nutrail spin-up: zero → self-funding donation server on LNVPS, paid in sats.
//
//   cd deploy && bun install && bun spinup.ts
//   bun spinup.ts --preflight   # no-money check: API reachable, auth works
//
// Flow: generate seedphrase locally (OS CSPRNG) → pick mint + region + payout
// address → LNVPS creates the VM and hands us a Lightning invoice → pay it
// from the terminal QR → box boots Docker + Caddy (auto-HTTPS on sslip.io)
// → server renews itself monthly by melting its ecash runway.

import * as p from "@clack/prompts";
import { randomBytes } from "crypto";
import { entropyToMnemonic } from "@scure/bip39";
import { wordlist } from "@scure/bip39/wordlists/english";
import { privateKeyFromSeedWords } from "nostr-tools/nip06";
import { getToken } from "nostr-tools/nip98";
import { finalizeEvent } from "nostr-tools/pure";
// @ts-ignore — no types shipped
import qrcode from "qrcode-terminal";

const API = process.env.LNVPS_API ?? "https://api.lnvps.net";
const GiB = 1024 ** 3;
const SPEC = { cpu: 1, memory: 1 * GiB, disk: 10 * GiB, disk_type: "ssd", disk_interface: "pcie" };
const IMAGE = process.env.NUTRAIL_IMAGE ?? "ghcr.io/kelbie/nutrail:latest";

// ---------------------------------------------------------------- api client
let secretKey: Uint8Array | null = null;
const sign = (e: any) => finalizeEvent(e, secretKey!);

async function api(method: "GET" | "POST", path: string, body?: unknown, authed = true) {
  const url = `${API}${path}`;
  const headers: Record<string, string> = { "content-type": "application/json" };
  if (authed) headers.Authorization = await getToken(url, method, sign, true);
  const res = await fetch(url, { method, headers, body: body ? JSON.stringify(body) : undefined });
  const json: any = await res.json().catch(() => ({ error: `HTTP ${res.status}` }));
  if (json.error) throw new Error(`${method} ${path}: ${json.error}`);
  return json.data;
}

const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));
const sats = (msats: number) => Math.ceil(msats / 1000);

async function regionOptions() {
  const t = await api("GET", "/api/v1/vm/templates", undefined, false);
  const regions: { label: string; hint: string; value: number }[] = [];
  for (const ct of t.custom_template ?? []) {
    try {
      const q = await api("POST", "/api/v1/vm/custom-template/price", { pricing_id: ct.id, ...SPEC }, false);
      const btc = (q.currency === "BTC" ? q : q.other_price?.find((o: any) => o.currency === "BTC")) ?? {};
      const eur = (q.currency === "EUR" ? q : q.other_price?.find((o: any) => o.currency === "EUR")) ?? {};
      regions.push({
        value: ct.id,
        label: ct.region.name,
        hint: `~${sats(btc.amount ?? 0).toLocaleString()} sats/mo (€${((eur.amount ?? 0) / 100).toFixed(2)})`,
      });
    } catch { /* region without a quote: skip */ }
  }
  // London first: Dublin has absorbed most of LNVPS's historical incidents.
  return regions.sort((a, b) => (a.label.includes("London") ? -1 : b.label.includes("London") ? 1 : 0));
}

// -------------------------------------------------------------------- main
const preflight = process.argv.includes("--preflight");

p.intro("nutrail → LNVPS");

if (preflight) {
  const entropy = randomBytes(16);
  const mnemonic = entropyToMnemonic(entropy, wordlist);
  secretKey = privateKeyFromSeedWords(mnemonic);
  const vms = await api("GET", "/api/v1/vm");
  const regions = await regionOptions();
  p.log.success(`auth ok (fresh key sees ${vms.length} VMs)`);
  for (const r of regions) p.log.info(`${r.label}: ${r.hint}`);
  p.outro("preflight passed — API reachable, NIP-98 accepted, prices quoted");
  process.exit(0);
}

// 1. seedphrase — generated locally, never sent anywhere except as entropy to your own VM
const entropy = randomBytes(16);
const entropyHex = entropy.toString("hex");
const mnemonic = entropyToMnemonic(entropy, wordlist);
secretKey = privateKeyFromSeedWords(mnemonic);

p.note(mnemonic.split(" ").map((w, i) => `${String(i + 1).padStart(2)}. ${w}`).join("\n"),
  "Your seedphrase — WRITE THESE 12 WORDS ON PAPER");
p.log.warn("These words are the ecash wallet AND the hosting account identity.");
const written = await p.confirm({ message: "Written down?" });
if (p.isCancel(written) || !written) { p.cancel("Come back when it's on paper."); process.exit(1); }

// 2. mint
const mint = await p.select({
  message: "Which Cashu mint should receive donations?",
  options: [
    { value: "https://mint.minibits.cash/Bitcoin", label: "mint.minibits.cash/Bitcoin", hint: "real sats — recommended" },
    { value: "https://testnut.cashudevkit.org", label: "testnut.cashudevkit.org", hint: "CDK test mint — FAKE sats, cannot self-pay hosting" },
    { value: "custom", label: "Another mint…" },
  ],
});
if (p.isCancel(mint)) process.exit(1);
let mintUrl = mint as string;
if (mintUrl === "custom") {
  const m = await p.text({ message: "Mint URL", placeholder: "https://…" });
  if (p.isCancel(m)) process.exit(1);
  mintUrl = (m as string).trim();
}
const isTestnut = mintUrl.includes("testnut");
if (isTestnut) p.log.warn("Testnut sats are worthless: the server CANNOT pay its own renewals — you'll need to renew manually.");

// 3. payout address
const payout = await p.text({
  message: "Lightning address donations get swept to",
  placeholder: "you@wallet.com",
  validate: (v) => (v.includes("@") && v.includes(".") ? undefined : "Needs to look like name@domain"),
});
if (p.isCancel(payout)) process.exit(1);

// 4. region (live prices)
const s = p.spinner();
s.start("Quoting regions");
const regions = await regionOptions();
s.stop("Regions quoted");
const pricingId = await p.select({ message: "Region (1 vCPU / 1GB / 10GB NVMe)", options: regions });
if (p.isCancel(pricingId)) process.exit(1);

// 5. ssh key + image
s.start("Registering SSH key");
const keyPath = `${process.cwd()}/nutrail-ssh`;
await Bun.spawn(["ssh-keygen", "-t", "ed25519", "-f", keyPath, "-N", "", "-C", "nutrail", "-q"]).exited;
const pubkey = (await Bun.file(`${keyPath}.pub`).text()).trim();
const sshKey = await api("POST", "/api/v1/ssh-key", { name: `nutrail-${Date.now()}`, key_data: pubkey });
const images = await api("GET", "/api/v1/image", undefined, false);
const image = images.find((i: any) => i.distribution === "ubuntu" && i.version?.startsWith("24.04")) ?? images[0];
s.stop(`SSH key #${sshKey.id} registered, image: ${image.distribution} ${image.version}`);

// 6. create VM → first-month invoice from LNVPS, paid by YOU in the terminal
s.start("Creating VM");
const vm = await api("POST", "/api/v1/vm/custom-template", {
  pricing_id: pricingId, ...SPEC, image_id: image.id, ssh_key_id: sshKey.id,
});
s.stop(`VM #${vm.id} created (unpaid)`);

const payment = await api("GET", `/api/v1/vm/${vm.id}/renew?method=lightning`);
const invoice: string = payment.data.lightning;
const costMsats: number = payment.amount;

console.log();
qrcode.generate(invoice, { small: true });
console.log(`\n  ${invoice}\n`);
p.log.step(`Pay ${sats(costMsats).toLocaleString()} sats for the first month with any lightning wallet`);

s.start("Waiting for payment");
for (;;) {
  await sleep(4000);
  const st = await api("GET", `/api/v1/payment/${payment.id}`);
  if (st.is_paid) break;
}
s.stop("Paid — LNVPS is provisioning");

// 7. wait for IP
s.start("Waiting for the VM to come up");
let ip = "";
for (let i = 0; i < 90 && !ip; i++) {
  await sleep(5000);
  const v = await api("GET", `/api/v1/vm/${vm.id}`);
  ip = v.ip_assignments?.[0]?.ip?.split("/")[0] ?? "";
}
if (!ip) { p.cancel("Timed out waiting for an IP — check https://lnvps.net"); process.exit(1); }
const domain = `${ip.replaceAll(".", "-")}.sslip.io`;
s.stop(`Up: ${ip} → https://${domain}`);

// 8. bootstrap over SSH (LNVPS has no cloud-init user-data)
const setupToken = randomBytes(16).toString("hex");
const env = [
  `SETUP_TOKEN=${setupToken}`,
  `SEED_ENTROPY_HEX=${entropyHex}`,
  `MINT_URL=${mintUrl}`,
  `PAYOUT_LN_ADDRESS=${payout}`,
  `LNVPS_VM_ID=${vm.id}`,
  `RENEWAL_COST_MSATS=${costMsats}`,
  `PUBLIC_URL=https://${domain}`,
  `DATA_DIR=/data`,
].join("\n");

const remote = `#!/bin/bash
set -e
command -v docker >/dev/null || curl -fsSL https://get.docker.com | sh
mkdir -p /opt/nutrail && cd /opt/nutrail
cat > .env <<'ENV'
${env}
ENV
cat > Caddyfile <<'CADDY'
${domain} {
    reverse_proxy nutrail:3000
}
CADDY
cat > docker-compose.yml <<'COMPOSE'
services:
  nutrail:
    image: ${IMAGE}
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
`;

s.start("Bootstrapping over SSH (docker install + start)");
const b64 = Buffer.from(remote).toString("base64");
const sshArgs = ["-o", "StrictHostKeyChecking=accept-new", "-o", "ConnectTimeout=10", "-i", keyPath];
let booted = false;
for (let i = 0; i < 30 && !booted; i++) {
  const proc = Bun.spawn(
    ["ssh", ...sshArgs, `${image.default_username}@${ip}`, `echo ${b64} | base64 -d | sudo bash`],
    { stdout: "ignore", stderr: "pipe" },
  );
  booted = (await proc.exited) === 0;
  if (!booted) await sleep(10000);
}
if (!booted) { p.cancel(`SSH bootstrap failed — try manually: ssh -i ${keyPath} ${image.default_username}@${ip}`); process.exit(1); }
s.stop("Bootstrapped");

// 9. wait for HTTPS
s.start("Waiting for nutrail + TLS certificate");
let ready = false;
for (let i = 0; i < 60 && !ready; i++) {
  await sleep(10000);
  ready = await fetch(`https://${domain}/ping`, { signal: AbortSignal.timeout(5000) })
    .then((r) => r.ok).catch(() => false);
}
s.stop(ready ? "Live" : "Not responding yet — give it a few minutes");

// 10. secrets + summary
const secretsFile = `nutrail-secrets-${vm.id}.txt`;
await Bun.write(secretsFile, `mnemonic: ${mnemonic}\nentropy_hex: ${entropyHex}\nsetup_token: ${setupToken}\nvm_id: ${vm.id}\nssh: ssh -i ${keyPath} ${image.default_username}@${ip}\n`);
await Bun.spawn(["chmod", "600", secretsFile]).exited;

p.note(
  `donate page  https://${domain}/donate
admin        https://${domain}/admin?token=${setupToken}
embed        <script src="https://${domain}/donate/widget.js" defer></script>

Sweeps to ${payout}, keeps ~${Math.ceil((costMsats / 1000) * 1.5).toLocaleString()} sats
(1.5× monthly cost) as ecash runway, and renews itself ~3 days
before expiry by melting that runway.${isTestnut ? "\n\n⚠ testnut mint: self-renewal will NOT work — renew manually." : ""}

Secrets in ./${secretsFile} — delete after the words are on paper.`,
  "done",
);
p.outro(`Monthly cost: ~${sats(costMsats).toLocaleString()} sats, paid by the server itself.`);
