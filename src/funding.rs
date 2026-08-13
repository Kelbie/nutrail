//! Self-funding against LNVPS (lnvps.net): the server pays its own hosting
//! renewal out of the ecash runway the sweeper holds back.
//!
//! Identity: the LNVPS account is a nostr key derived from the wallet mnemonic
//! (NIP-06, m/44'/1237'/0'/0/0) — the same words that back the ecash back the
//! hosting account. Requests are signed per-call with NIP-98.
//!
//! Loop: every 6h read the VM's expiry; within the renewal window, fetch a
//! renewal invoice (`GET /api/v1/vm/{id}/renew?method=lightning`, amount in
//! millisats) and melt ecash to pay it. After every renewal the runway reserve
//! is re-derived as `monthly cost × 1.5` so a bitcoin price move between
//! renewals stays covered.

use std::str::FromStr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bip39::Mnemonic;
use bitcoin::bip32::{DerivationPath, Xpriv};
use bitcoin::hashes::{sha256, Hash};
use bitcoin::key::Keypair;
use bitcoin::secp256k1::{Message, Secp256k1};
use bitcoin::Network;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::PaymentMethod;
use serde_json::{json, Value};

use crate::{melt_by_quote_id, App};

const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// Renew when fewer than this many days remain.
const RENEW_WINDOW_DAYS: f64 = 3.0;
/// Runway multiplier over one month's cost ("50% over").
const RESERVE_FACTOR_NUM: u64 = 3;
const RESERVE_FACTOR_DEN: u64 = 2;

pub struct FundingConfig {
    pub api: String,
    pub vm_id: u64,
}

impl FundingConfig {
    pub fn from_env() -> Option<Self> {
        let vm_id = std::env::var("LNVPS_VM_ID").ok()?.trim().parse().ok()?;
        Some(Self {
            api: std::env::var("LNVPS_API")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "https://api.lnvps.net".into()),
            vm_id,
        })
    }
}

pub fn reserve_from_cost_msats(cost_msats: u64) -> u64 {
    (cost_msats / 1000) * RESERVE_FACTOR_NUM / RESERVE_FACTOR_DEN
}

// ---------------------------------------------------------------------------
// NIP-06 key + NIP-98 request signing
// ---------------------------------------------------------------------------

pub fn nostr_keypair(mnemonic: &str) -> Result<Keypair, String> {
    let mnemonic = Mnemonic::from_str(mnemonic).map_err(|e| e.to_string())?;
    let seed = mnemonic.to_seed_normalized("");
    let secp = Secp256k1::new();
    let master = Xpriv::new_master(Network::Bitcoin, &seed).map_err(|e| e.to_string())?;
    let path = DerivationPath::from_str("m/44'/1237'/0'/0/0").map_err(|e| e.to_string())?;
    let child = master.derive_priv(&secp, &path).map_err(|e| e.to_string())?;
    Ok(Keypair::from_secret_key(&secp, &child.private_key))
}

pub fn nostr_pubkey_hex(keypair: &Keypair) -> String {
    keypair.x_only_public_key().0.to_string()
}

fn nip98_header(keypair: &Keypair, url: &str, method: &str) -> Result<String, String> {
    let secp = Secp256k1::new();
    let pubkey = nostr_pubkey_hex(keypair);
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs();
    let tags = json!([["u", url], ["method", method]]);

    // NIP-01 canonical form: [0, pubkey, created_at, kind, tags, content]
    let canonical = serde_json::to_string(&json!([0, pubkey, created_at, 27235, tags, ""]))
        .map_err(|e| e.to_string())?;
    let id = sha256::Hash::hash(canonical.as_bytes());
    let msg = Message::from_digest(id.to_byte_array());
    let sig = secp.sign_schnorr(&msg, keypair);

    let event = json!({
        "id": id.to_string(),
        "pubkey": pubkey,
        "created_at": created_at,
        "kind": 27235,
        "tags": tags,
        "content": "",
        "sig": sig.to_string(),
    });
    Ok(format!("Nostr {}", b64(event.to_string().as_bytes())))
}

fn b64(data: &[u8]) -> String {
    // Standard alphabet with padding, RFC 4648.
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

async fn api_get(
    client: &reqwest::Client,
    keypair: &Keypair,
    url: &str,
) -> Result<Value, String> {
    let auth = nip98_header(keypair, url, "GET")?;
    let body: Value = client
        .get(url)
        .header("Authorization", auth)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;
    if let Some(err) = body.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    Ok(body["data"].clone())
}

/// Parse an RFC3339 timestamp ("2026-12-31T23:59:59Z", optional fractional
/// seconds / offset) into a unix timestamp. Minimal on purpose.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, u32, u32) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    let time_part = rest
        .trim_end_matches('Z')
        .split(['+', '.'])
        .next()?;
    let mut t = time_part.split(':');
    let (hh, mm, ss): (u64, u64, u64) = (
        t.next()?.parse().ok()?,
        t.next()?.parse().ok()?,
        t.next().unwrap_or("0").parse().ok()?,
    );
    // Days since epoch via civil-from-days inverse (Howard Hinnant's algorithm).
    let y = y - i64::from(m <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as u64;
    let mp = u64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + u64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era as u64 * 146097 + doe - 719468;
    Some(days * 86400 + hh * 3600 + mm * 60 + ss)
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

pub async fn run(app: Arc<App>, cfg: FundingConfig) {
    let keypair = match nostr_keypair(&app.mnemonic) {
        Ok(k) => k,
        Err(e) => {
            tracing::error!("funding: could not derive nostr key: {e}");
            return;
        }
    };
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("funding: http client: {e}");
            return;
        }
    };
    tracing::info!(
        "self-funding enabled: LNVPS vm {} as nostr key {}",
        cfg.vm_id,
        nostr_pubkey_hex(&keypair)
    );

    loop {
        if let Err(e) = tick(&app, &cfg, &client, &keypair).await {
            tracing::warn!("funding check failed (will retry): {e}");
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

async fn tick(
    app: &App,
    cfg: &FundingConfig,
    client: &reqwest::Client,
    keypair: &Keypair,
) -> Result<(), String> {
    let vm = api_get(client, keypair, &format!("{}/api/v1/vm/{}", cfg.api, cfg.vm_id)).await?;
    let expires = vm["expires"].as_str().unwrap_or_default();
    let expires_ts = parse_rfc3339(expires).ok_or_else(|| format!("bad expires: {expires}"))?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
    let days_left = (expires_ts.saturating_sub(now)) as f64 / 86400.0;

    *app.runway.write().await = Some(json!({
        "provider": "lnvps",
        "vmId": cfg.vm_id,
        "expires": expires,
        "daysLeft": (days_left * 10.0).round() / 10.0,
        "state": vm["status"]["state"],
    }));
    tracing::info!("hosting runway: VM {} expires {} ({days_left:.1} days)", cfg.vm_id, expires);

    if days_left >= RENEW_WINDOW_DAYS {
        return Ok(());
    }

    tracing::info!("inside renewal window — fetching lightning renewal invoice");
    let payment = api_get(
        client,
        keypair,
        &format!("{}/api/v1/vm/{}/renew?method=lightning", cfg.api, cfg.vm_id),
    )
    .await?;
    let invoice = payment["data"]["lightning"].as_str().unwrap_or_default().to_string();
    let amount_msats = payment["amount"].as_u64().unwrap_or(0);
    if !invoice.to_lowercase().starts_with("ln") {
        return Err(format!("no bolt11 in renewal payment: {payment}"));
    }

    let quote = app
        .wallet
        .melt_quote(PaymentMethod::Known(KnownMethod::Bolt11), invoice, None, None)
        .await
        .map_err(|e| format!("melt quote for renewal failed: {e}"))?;
    let needed = u64::from(quote.amount) + u64::from(quote.fee_reserve);
    let have = app.wallet.total_balance().await.map(u64::from).unwrap_or(0);
    if have < needed {
        app.emit(
            "runway_underfunded",
            json!({ "neededSats": needed, "haveSats": have, "daysLeft": days_left }),
        );
        return Err(format!("runway underfunded: renewal needs {needed} sats, wallet has {have}"));
    }

    let paid = melt_by_quote_id(app, &quote.id)
        .await
        .map_err(|e| format!("renewal melt failed: {e}"))?;

    // Refresh the reserve: latest monthly cost × 1.5.
    if amount_msats > 0 {
        let reserve = reserve_from_cost_msats(amount_msats);
        app.reserve_sats.store(reserve, Ordering::Relaxed);
        tracing::info!("reserve re-derived from renewal cost: {reserve} sats");
    }
    let added_days = payment["time"].as_u64().unwrap_or(0) / 86400;
    tracing::info!("paid hosting renewal: {paid} sats for ~{added_days} days");
    app.emit("self_funded", json!({ "sats": paid, "daysAdded": added_days }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// NIP-06 test vector from the spec.
    #[test]
    fn nip06_vector() {
        let kp = nostr_keypair(
            "leader monkey parrot ring guide accident before fence cannon height naive bean",
        )
        .unwrap();
        assert_eq!(
            nostr_pubkey_hex(&kp),
            "17162c921dc4d2518f9a101db33695df1afb56ab82f5ff3e5da6eec3ca5cd917"
        );
    }

    #[test]
    fn b64_roundtrip() {
        assert_eq!(b64(b"hello"), "aGVsbG8=");
        assert_eq!(b64(b"hi"), "aGk=");
        assert_eq!(b64(b"abc"), "YWJj");
    }

    #[test]
    fn rfc3339() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-08-13T12:00:00Z"), Some(1786622400));
    }

    /// Live check that LNVPS accepts our NIP-98 signing (fresh key → empty VM
    /// list, not a 401). Run explicitly: cargo test lnvps_auth_live -- --ignored
    #[tokio::test]
    #[ignore]
    async fn lnvps_auth_live() {
        let mnemonic = Mnemonic::generate(12).unwrap().to_string();
        let keypair = nostr_keypair(&mnemonic).unwrap();
        let client = reqwest::Client::new();
        let vms = api_get(&client, &keypair, "https://api.lnvps.net/api/v1/vm")
            .await
            .unwrap();
        assert!(vms.as_array().is_some(), "expected a VM array, got: {vms}");
    }
}
