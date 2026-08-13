//! Self-funding: the server pays its own BitLaunch hosting bill out of the
//! ecash runway the sweeper holds back (`RESERVE_SATS`).
//!
//! Every few hours: read the BitLaunch account balance (`GET /user`, USD mils);
//! when it drops below the threshold, create a Lightning funding transaction
//! (`POST /transactions`) and melt ecash to pay the returned BOLT-11 invoice.
//! BitLaunch bills the VPS hourly against that prepaid balance, so donations
//! literally keep the server alive.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};

use crate::{melt_by_quote_id, App};
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::PaymentMethod;

const API: &str = "https://app.bitlaunch.io/api";
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

pub struct FundingConfig {
    pub token: String,
    /// Refill when the BitLaunch balance drops below this (USD).
    pub min_balance_usd: f64,
    /// Size of each top-up (USD). BitLaunch minimum is $20.
    pub topup_usd: u64,
}

impl FundingConfig {
    pub fn from_env() -> Option<Self> {
        let token = std::env::var("BL_API_TOKEN").ok().filter(|s| !s.is_empty())?;
        Some(Self {
            token,
            min_balance_usd: std::env::var("BL_MIN_BALANCE_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(5.0),
            topup_usd: std::env::var("BL_TOPUP_USD")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(20)
                .max(20),
        })
    }
}

async fn get_json(client: &reqwest::Client, token: &str, path: &str) -> Result<Value, String> {
    client
        .get(format!("{API}{path}"))
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// Account snapshot used by the loop and exposed at GET /runway.
pub async fn account_status(client: &reqwest::Client, token: &str) -> Result<Value, String> {
    let user = get_json(client, token, "/user").await?;
    let balance_mils = user["balance"].as_f64().unwrap_or(0.0);
    let cost_per_hr_mils = user["costPerHr"].as_f64().unwrap_or(0.0);
    let hours_left = if cost_per_hr_mils > 0.0 {
        balance_mils / cost_per_hr_mils
    } else {
        f64::INFINITY
    };
    Ok(json!({
        "balanceUsd": balance_mils / 1000.0,
        "costPerHrUsd": cost_per_hr_mils / 1000.0,
        "daysLeft": if hours_left.is_finite() { Value::from(hours_left / 24.0) } else { Value::Null },
    }))
}

pub async fn run(app: Arc<App>, cfg: FundingConfig) {
    let client = match reqwest::Client::builder().timeout(Duration::from_secs(30)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("funding: could not build http client: {e}");
            return;
        }
    };
    tracing::info!(
        "self-funding enabled: top up ${} when BitLaunch balance < ${}",
        cfg.topup_usd,
        cfg.min_balance_usd
    );

    loop {
        if let Err(e) = tick(&app, &cfg, &client).await {
            tracing::warn!("funding check failed (will retry): {e}");
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

async fn tick(app: &App, cfg: &FundingConfig, client: &reqwest::Client) -> Result<(), String> {
    let status = account_status(client, &cfg.token).await?;
    let balance_usd = status["balanceUsd"].as_f64().unwrap_or(0.0);
    tracing::info!(
        "hosting runway: ${balance_usd:.2} on BitLaunch ({} days left)",
        status["daysLeft"].as_f64().map(|d| format!("{d:.1}")).unwrap_or_else(|| "∞".into())
    );

    if balance_usd >= cfg.min_balance_usd {
        return Ok(());
    }

    tracing::info!("balance below ${} — creating ${} lightning top-up", cfg.min_balance_usd, cfg.topup_usd);
    let tx: Value = client
        .post(format!("{API}/transactions"))
        .bearer_auth(&cfg.token)
        .json(&json!({
            "amountUsd": cfg.topup_usd,
            "cryptoSymbol": "BTC",
            "lightningNetwork": true,
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())?;

    let invoice = tx["address"].as_str().unwrap_or_default().to_string();
    if !invoice.to_lowercase().starts_with("ln") {
        return Err(format!(
            "expected a BOLT-11 invoice in transaction.address, got: {:?} (statusUrl: {})",
            &invoice.chars().take(24).collect::<String>(),
            tx["statusUrl"].as_str().unwrap_or("-")
        ));
    }

    let quote = app
        .wallet
        .melt_quote(
            PaymentMethod::Known(KnownMethod::Bolt11),
            invoice.clone(),
            None,
            None,
        )
        .await
        .map_err(|e| format!("melt quote for hosting invoice failed: {e}"))?;
    let needed = u64::from(quote.amount) + u64::from(quote.fee_reserve);
    let have = app.wallet.total_balance().await.map(u64::from).unwrap_or(0);
    if have < needed {
        app.emit(
            "runway_underfunded",
            json!({ "neededSats": needed, "haveSats": have, "topupUsd": cfg.topup_usd }),
        );
        return Err(format!(
            "runway underfunded: hosting top-up needs {needed} sats, wallet has {have}"
        ));
    }

    let paid = melt_by_quote_id(app, &quote.id)
        .await
        .map_err(|e| format!("hosting invoice melt failed: {e}"))?;
    tracing::info!("paid ${} hosting top-up with {paid} sats of ecash", cfg.topup_usd);
    app.emit("self_funded", json!({ "usd": cfg.topup_usd, "sats": paid }));
    Ok(())
}
