//! nutrail — minimal CDK-based donation wallet for Railway.
//!
//! Receives donations via a Cashu mint (bolt11 invoices + pasted ecash tokens)
//! and auto-melts the balance to the owner's self-custodial lightning address.
//!
//! The wallet API mirrors cocod's route table (https://github.com/Egge21M/cocod):
//! `/ping`, `/status`, `/balance`, `/receive/{cashu,bolt11}`, `/send/{cashu,bolt11}`,
//! `/mints/{list,info}`, `/history`, `/events` — same `{output}` / `{error}` JSON
//! envelope. Donation-widget routes live under `/donate/*` and are public;
//! everything else requires `Authorization: Bearer $SETUP_TOKEN`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bip39::Mnemonic;
use cdk::mint_url::MintUrl;
use cdk::nuts::nut00::KnownMethod;
use cdk::nuts::nut18::{PaymentRequest, PaymentRequestPayload, Transport, TransportType};
use cdk::nuts::{CurrencyUnit, PaymentMethod};
use cdk::wallet::{ReceiveOptions, SendOptions, Wallet};
use cdk::Amount;
use cdk_sqlite::WalletSqliteDatabase;
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::StreamExt;
use tower_http::cors::{Any, CorsLayer};

mod funding;

const DEFAULT_MINT: &str = "https://testnut.cashudevkit.org";

struct App {
    wallet: Wallet,
    mint_url: String,
    mnemonic: String,
    setup_token: String,
    payout_address: Option<String>,
    sweep_threshold: u64,
    /// Sats held back from every sweep — the hosting runway kept as ecash.
    /// Re-derived as monthly cost × 1.1 after every renewal.
    reserve_sats: AtomicU64,
    /// Latest hosting status from the funding loop, served at GET /runway.
    runway: tokio::sync::RwLock<Option<serde_json::Value>>,
    events: broadcast::Sender<String>,
    onchain_supported: bool,
    public_url: Option<String>,
}

impl App {
    /// External base URL for links the donor's wallet must reach (NUT-18 target).
    fn base_url(&self, headers: &HeaderMap) -> String {
        if let Some(url) = &self.public_url {
            return url.trim_end_matches('/').to_string();
        }
        let host = headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("localhost");
        let proto = headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(if host.starts_with("localhost") { "http" } else { "https" });
        format!("{proto}://{host}")
    }
}

impl App {
    fn emit(&self, kind: &str, data: Value) {
        let _ = self
            .events
            .send(json!({ "type": kind, "data": data }).to_string());
    }
}

fn ok(output: Value) -> Response {
    Json(json!({ "output": output })).into_response()
}

fn err(status: StatusCode, msg: impl ToString) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}

// ---------------------------------------------------------------------------
// Seed handling: read from volume, restore from env, or generate on first boot
// ---------------------------------------------------------------------------

fn load_or_create_mnemonic(data_dir: &PathBuf) -> anyhow_lite::Result<(Mnemonic, bool)> {
    let path = data_dir.join("mnemonic.txt");
    if path.exists() {
        let words = std::fs::read_to_string(&path)?;
        return Ok((Mnemonic::from_str(words.trim())?, false));
    }

    // SEED_ENTROPY_HEX: raw entropy handed in by the provisioning script
    // (generated on the operator's machine from the OS CSPRNG). 16-32 bytes,
    // hex-encoded; 32 bytes = 24 words.
    let entropy_hex = std::env::var("SEED_ENTROPY_HEX").ok().filter(|s| !s.trim().is_empty());
    let (mnemonic, restored) = if let Some(hex) = entropy_hex {
        let bytes = decode_hex(hex.trim())
            .map_err(|e| format!("SEED_ENTROPY_HEX is not valid hex: {e}"))?;
        if ![16, 20, 24, 28, 32].contains(&bytes.len()) {
            return Err(format!(
                "SEED_ENTROPY_HEX must be 16-32 bytes (multiple of 4); got {}",
                bytes.len()
            )
            .into());
        }
        (Mnemonic::from_entropy(&bytes)?, false)
    } else {
        match std::env::var("RESTORE_MNEMONIC") {
            Ok(words) if !words.trim().is_empty() => (Mnemonic::from_str(words.trim())?, true),
            _ => (Mnemonic::generate(12)?, false),
        }
    };

    // OS CSPRNG entropy via bip39's rand feature; never derived from anything else.
    std::fs::write(&path, mnemonic.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok((mnemonic, restored))
}

// Tiny error alias so we don't pull in anyhow for three call sites.
mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}

fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    if s.len() % 2 != 0 {
        return Err("odd length".into());
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn authed(app: &App, headers: &HeaderMap, query_token: Option<&str>) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    bearer == Some(app.setup_token.as_str()) || query_token == Some(app.setup_token.as_str())
}

macro_rules! require_auth {
    ($app:expr, $headers:expr, $token:expr) => {
        if !authed($app, $headers, $token) {
            return err(StatusCode::UNAUTHORIZED, "Unauthorized");
        }
    };
}

// ---------------------------------------------------------------------------
// cocod-mirrored wallet routes (auth required)
// ---------------------------------------------------------------------------

async fn ping() -> Response {
    ok(json!("pong"))
}

async fn status(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    ok(json!({
        "status": "unlocked",
        "mintUrl": app.mint_url,
        "payoutAddress": app.payout_address,
        "sweepThresholdSats": app.sweep_threshold,
        "reserveSats": app.reserve_sats.load(Ordering::Relaxed),
    }))
}

async fn balance(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    match app.wallet.total_balance().await {
        Ok(amount) => ok(json!({ app.mint_url.clone(): { "sats": u64::from(amount) } })),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get balance: {e}"),
        ),
    }
}

#[derive(serde::Deserialize)]
struct ReceiveCashuBody {
    token: String,
}

async fn receive_cashu(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<ReceiveCashuBody>,
) -> Response {
    require_auth!(&app, &headers, None);
    receive_cashu_inner(&app, &body.token).await
}

async fn receive_cashu_inner(app: &App, token: &str) -> Response {
    match app.wallet.receive(token, ReceiveOptions::default()).await {
        Ok(amount) => {
            app.emit("received", json!({ "method": "cashu", "sats": u64::from(amount) }));
            ok(json!(format!("Received {amount}")))
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("Receive failed: {e}")),
    }
}

#[derive(serde::Deserialize)]
struct ReceiveBolt11Body {
    amount: u64,
}

async fn receive_bolt11(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<ReceiveBolt11Body>,
) -> Response {
    require_auth!(&app, &headers, None);
    receive_bolt11_inner(&app, body.amount).await
}

async fn receive_bolt11_inner(app: &App, amount: u64) -> Response {
    if amount == 0 || amount > 10_000_000 {
        return err(StatusCode::BAD_REQUEST, "Invalid amount");
    }
    match app
        .wallet
        .mint_quote(
            PaymentMethod::Known(KnownMethod::Bolt11),
            Some(Amount::from(amount)),
            None,
            None,
        )
        .await
    {
        Ok(quote) => ok(json!({ "request": quote.request, "id": quote.id, "amount": amount })),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to create invoice: {e}"),
        ),
    }
}

async fn quote_status_inner(app: &App, id: &str) -> Response {
    match app.wallet.check_mint_quote_status(id).await {
        Ok(quote) => ok(json!({ "state": quote.state.to_string() })),
        Err(e) => err(StatusCode::NOT_FOUND, format!("Quote not found: {e}")),
    }
}

#[derive(serde::Deserialize)]
struct SendCashuBody {
    amount: u64,
}

async fn send_cashu(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SendCashuBody>,
) -> Response {
    require_auth!(&app, &headers, None);
    let prepared = match app
        .wallet
        .prepare_send(Amount::from(body.amount), SendOptions::default())
        .await
    {
        Ok(p) => p,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("Send failed: {e}")),
    };
    match prepared.confirm(None).await {
        Ok(token) => ok(json!(token.to_string())),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Send failed: {e}"),
        ),
    }
}

#[derive(serde::Deserialize)]
struct SendBolt11Body {
    invoice: String,
}

async fn send_bolt11(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Json(body): Json<SendBolt11Body>,
) -> Response {
    require_auth!(&app, &headers, None);
    let quote = match app
        .wallet
        .melt_quote(
            PaymentMethod::Known(KnownMethod::Bolt11),
            body.invoice.clone(),
            None,
            None,
        )
        .await
    {
        Ok(q) => q,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("Payment failed: {e}")),
    };
    match melt_by_quote_id(&app, &quote.id).await {
        Ok(paid) => ok(json!(format!("Paid {paid} sats"))),
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Payment failed: {e}"),
        ),
    }
}

async fn melt_by_quote_id(app: &App, quote_id: &str) -> Result<u64, cdk::Error> {
    let prepared = app.wallet.prepare_melt(quote_id, HashMap::new()).await?;
    let amount = u64::from(prepared.amount());
    prepared.confirm().await?;
    Ok(amount)
}

async fn runway(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    let wallet_sats = app.wallet.total_balance().await.map(u64::from).unwrap_or(0);
    let hosting = app.runway.read().await.clone();
    ok(json!({
        "walletSats": wallet_sats,
        "reserveSats": app.reserve_sats.load(Ordering::Relaxed),
        "hosting": hosting,
    }))
}

async fn mints_list(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    ok(json!([app.mint_url.clone()]))
}

async fn mints_info(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    match app.wallet.fetch_mint_info().await {
        Ok(info) => match serde_json::to_value(&info) {
            Ok(v) => ok(v),
            Err(e) => err(StatusCode::INTERNAL_SERVER_ERROR, e),
        },
        Err(e) => err(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Failed to get mint info: {e}"),
        ),
    }
}

async fn history(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    require_auth!(&app, &headers, None);
    // PoC: pending quotes only; full transaction history is a follow-up.
    let pending: Vec<Value> = app
        .wallet
        .get_pending_melt_quotes()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|q| json!({ "kind": "melt", "id": q.id, "sats": u64::from(q.amount), "state": q.state.to_string() }))
        .collect();
    ok(json!(pending))
}

async fn events(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    // EventSource can't set headers, so ?token= is accepted here like /admin.
    require_auth!(&app, &headers, q.token.as_deref());
    let rx = app.events.subscribe();
    let stream = BroadcastStream::new(rx).filter_map(|msg| match msg {
        Ok(data) => Some(Ok::<SseEvent, std::convert::Infallible>(
            SseEvent::default().data(data),
        )),
        Err(_) => None,
    });
    Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(5)))
        .into_response()
}

// ---------------------------------------------------------------------------
// Public donation routes (CORS *, no auth)
// ---------------------------------------------------------------------------

async fn donate_config(State(app): State<Arc<App>>, headers: HeaderMap) -> Response {
    let mut methods = vec!["lightning", "cashu"];
    if app.onchain_supported {
        methods.push("onchain");
    }
    ok(json!({
        "methods": methods,
        "mint": app.mint_url,
        "creq": build_creq(&app, &headers),
    }))
}

fn build_creq(app: &App, headers: &HeaderMap) -> Option<String> {
    let mint_url = MintUrl::from_str(&app.mint_url).ok()?;
    let request = PaymentRequest::builder()
        .payment_id("donation")
        .unit(CurrencyUnit::Sat)
        .add_mint(mint_url)
        .description("Donation")
        .add_transport(Transport {
            _type: TransportType::HttpPost,
            target: format!("{}/donate/nut18", app.base_url(headers)),
            tags: vec![],
        })
        .build();
    Some(request.to_string())
}

#[derive(serde::Deserialize)]
struct DonateQuoteBody {
    method: String,
    amount: Option<u64>,
}

async fn donate_quote(
    State(app): State<Arc<App>>,
    Json(body): Json<DonateQuoteBody>,
) -> Response {
    match body.method.as_str() {
        "lightning" => match body.amount {
            Some(amount) => receive_bolt11_inner(&app, amount).await,
            None => err(StatusCode::BAD_REQUEST, "Amount required for lightning"),
        },
        "onchain" => {
            if !app.onchain_supported {
                return err(StatusCode::BAD_REQUEST, "Mint does not support onchain");
            }
            match app
                .wallet
                .mint_quote(
                    PaymentMethod::Known(KnownMethod::Onchain),
                    body.amount.map(Amount::from),
                    None,
                    None,
                )
                .await
            {
                Ok(quote) => {
                    ok(json!({ "request": quote.request, "id": quote.id, "kind": "onchain" }))
                }
                Err(e) => err(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("Failed to create onchain quote: {e}"),
                ),
            }
        }
        other => err(StatusCode::BAD_REQUEST, format!("Unknown method: {other}")),
    }
}

async fn donate_quote_status(State(app): State<Arc<App>>, Path(id): Path<String>) -> Response {
    quote_status_inner(&app, &id).await
}

/// NUT-18 HTTP POST transport target: the donor's wallet POSTs the payment
/// payload (proofs) here after paying the creq shown on the widget.
async fn donate_nut18(
    State(app): State<Arc<App>>,
    Json(payload): Json<PaymentRequestPayload>,
) -> Response {
    if payload.mint.to_string().trim_end_matches('/') != app.mint_url.trim_end_matches('/') {
        return err(StatusCode::BAD_REQUEST, "Proofs are not from the configured mint");
    }
    if payload.unit != CurrencyUnit::Sat {
        return err(StatusCode::BAD_REQUEST, "Only sat is accepted");
    }
    match app
        .wallet
        .receive_proofs(payload.proofs, ReceiveOptions::default(), payload.memo, None)
        .await
    {
        Ok(amount) => {
            app.emit("received", json!({ "method": "nut18", "sats": u64::from(amount) }));
            ok(json!(format!("Received {amount}")))
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("Receive failed: {e}")),
    }
}

async fn donate_cashu(
    State(app): State<Arc<App>>,
    Json(body): Json<ReceiveCashuBody>,
) -> Response {
    receive_cashu_inner(&app, &body.token).await
}

#[derive(serde::Deserialize)]
struct QrQuery {
    data: String,
}

async fn donate_qr(Query(q): Query<QrQuery>) -> Response {
    if q.data.len() > 4096 {
        return err(StatusCode::BAD_REQUEST, "Data too long");
    }
    match qrcode::QrCode::new(q.data.as_bytes()) {
        Ok(code) => {
            let svg = code
                .render::<qrcode::render::svg::Color>()
                .min_dimensions(220, 220)
                .build();
            ([(header::CONTENT_TYPE, "image/svg+xml")], svg).into_response()
        }
        Err(e) => err(StatusCode::BAD_REQUEST, format!("QR failed: {e}")),
    }
}

async fn donate_page() -> Html<&'static str> {
    Html(include_str!("../static/donate.html"))
}

async fn widget_js() -> Response {
    (
        [(header::CONTENT_TYPE, "application/javascript")],
        include_str!("../static/widget.js"),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// Admin page + mnemonic reveal
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
struct TokenQuery {
    token: Option<String>,
}

async fn admin_page(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    require_auth!(&app, &headers, q.token.as_deref());
    Html(include_str!("../static/admin.html")).into_response()
}

async fn admin_mnemonic(
    State(app): State<Arc<App>>,
    headers: HeaderMap,
    Query(q): Query<TokenQuery>,
) -> Response {
    require_auth!(&app, &headers, q.token.as_deref());
    ok(json!(app.mnemonic.clone()))
}

// ---------------------------------------------------------------------------
// Background sweeper: mint paid quotes, melt balance to the payout address
// ---------------------------------------------------------------------------

async fn sweeper(app: Arc<App>) {
    // Mandatory per CDK wallet README: resolve operations interrupted by a crash.
    match app.wallet.recover_incomplete_sagas().await {
        Ok(report) => tracing::info!(?report, "startup saga recovery complete"),
        Err(e) => tracing::warn!("startup saga recovery failed: {e}"),
    }

    loop {
        if let Ok(finalized) = app.wallet.finalize_pending_melts().await {
            for melt in &finalized {
                app.emit("melt_finalized", json!({ "state": melt.state().to_string() }));
            }
        }

        match app.wallet.mint_unissued_quotes().await {
            Ok(minted) if minted > Amount::ZERO => {
                tracing::info!("minted {minted} sats from paid quotes");
                app.emit("minted", json!({ "sats": u64::from(minted) }));
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("mint_unissued_quotes failed: {e}"),
        }

        if let Some(address) = &app.payout_address {
            let reserve = app.reserve_sats.load(Ordering::Relaxed);
            match app.wallet.total_balance().await {
                // Everything above the runway reserve gets swept out.
                Ok(bal) if u64::from(bal) >= reserve + app.sweep_threshold => {
                    let sweepable = u64::from(bal) - reserve;
                    if let Err(e) = sweep_to(&app, address, sweepable).await {
                        tracing::warn!("sweep failed (will retry): {e}");
                    }
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("balance check failed: {e}"),
            }
        }

        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

async fn sweep_to(app: &App, address: &str, balance_sats: u64) -> Result<(), cdk::Error> {
    // First quote at full balance to learn the fee reserve, then requote so
    // amount + fee_reserve fits inside the balance.
    let probe = app
        .wallet
        .melt_lightning_address_quote(address, Amount::from(balance_sats * 1000))
        .await?;
    let fee_reserve = u64::from(probe.fee_reserve);

    let quote = if u64::from(probe.amount) + fee_reserve <= balance_sats {
        probe
    } else {
        let target = balance_sats.saturating_sub(fee_reserve);
        if target == 0 {
            tracing::info!("balance {balance_sats} too small to cover melt fee reserve {fee_reserve}");
            return Ok(());
        }
        app.wallet
            .melt_lightning_address_quote(address, Amount::from(target * 1000))
            .await?
    };

    let paid = melt_by_quote_id(app, &quote.id).await?;
    tracing::info!("swept {paid} sats to {address}");
    app.emit("swept", json!({ "sats": paid, "to": address }));
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow_lite::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,nutrail=debug".into()),
        )
        .init();

    let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "./data".into()));
    std::fs::create_dir_all(&data_dir)?;

    let mint_url = std::env::var("MINT_URL").unwrap_or_else(|_| DEFAULT_MINT.into());
    let setup_token = std::env::var("SETUP_TOKEN").map_err(|_| {
        "SETUP_TOKEN env var is required (admin auth). Generate one: openssl rand -hex 16"
    })?;
    let payout_address = std::env::var("PAYOUT_LN_ADDRESS").ok().filter(|s| !s.is_empty());
    let sweep_threshold: u64 = std::env::var("SWEEP_THRESHOLD_SATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(100);
    // Initial reserve: derived from the first month's invoice when the spinup
    // CLI passes RENEWAL_COST_MSATS; the funding loop re-derives it after every
    // renewal (monthly cost × 1.1).
    let reserve_sats: u64 = std::env::var("RENEWAL_COST_MSATS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(funding::reserve_from_cost_msats)
        .or_else(|| std::env::var("RESERVE_SATS").ok().and_then(|v| v.parse().ok()))
        .unwrap_or(0);

    let (mnemonic, restored_from_env) = load_or_create_mnemonic(&data_dir)?;
    let seed = mnemonic.to_seed_normalized("");

    let db = WalletSqliteDatabase::new(&data_dir.join("wallet.sqlite")).await?;
    let wallet = Wallet::new(&mint_url, CurrencyUnit::Sat, Arc::new(db), seed, None)?;

    if restored_from_env {
        tracing::info!("RESTORE_MNEMONIC provided — running NUT-13 restore against {mint_url}");
        match wallet.restore().await {
            Ok(restored) => tracing::info!(
                "restore complete: {} sats unspent",
                u64::from(restored.unspent)
            ),
            Err(e) => tracing::warn!("restore failed: {e}"),
        }
    }

    // Capability detection: only offer methods the configured mint supports.
    let onchain_supported = match wallet.fetch_mint_info().await {
        Ok(info) => serde_json::to_value(&info)
            .ok()
            .and_then(|v| {
                v.pointer("/nuts/4/methods").map(|methods| {
                    methods
                        .as_array()
                        .map(|a| a.iter().any(|m| m["method"] == "onchain"))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false),
        Err(e) => {
            tracing::warn!("could not fetch mint info at boot: {e}");
            false
        }
    };
    tracing::info!("mint {mint_url}: onchain supported = {onchain_supported}");

    let public_url = std::env::var("PUBLIC_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("RAILWAY_PUBLIC_DOMAIN")
                .ok()
                .filter(|s| !s.is_empty())
                .map(|d| format!("https://{d}"))
        });

    let (events_tx, _) = broadcast::channel(256);
    let app = Arc::new(App {
        wallet,
        mint_url,
        mnemonic: mnemonic.to_string(),
        setup_token,
        payout_address,
        sweep_threshold,
        reserve_sats: AtomicU64::new(reserve_sats),
        runway: tokio::sync::RwLock::new(None),
        events: events_tx,
        onchain_supported,
        public_url,
    });

    if app.payout_address.is_none() {
        tracing::warn!("PAYOUT_LN_ADDRESS not set — donations will accumulate as ecash instead of auto-melting");
    }

    tokio::spawn(sweeper(app.clone()));

    if let Some(cfg) = funding::FundingConfig::from_env() {
        if app.reserve_sats.load(Ordering::Relaxed) == 0 {
            tracing::warn!("self-funding is on but the reserve is 0 — set RENEWAL_COST_MSATS or RESERVE_SATS so sweeps leave runway behind");
        }
        tokio::spawn(funding::run(app.clone(), cfg));
    }

    let public = Router::new()
        .route("/ping", get(ping))
        .route("/donate", get(donate_page))
        .route("/donate/widget.js", get(widget_js))
        .route("/donate/config", get(donate_config))
        .route("/donate/quote", post(donate_quote))
        .route("/donate/quote/{id}", get(donate_quote_status))
        .route("/donate/cashu", post(donate_cashu))
        .route("/donate/nut18", post(donate_nut18))
        .route("/donate/qr", get(donate_qr))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        );

    let wallet_api = Router::new()
        .route("/status", get(status))
        .route("/balance", get(balance))
        .route("/receive/cashu", post(receive_cashu))
        .route("/receive/bolt11", post(receive_bolt11))
        .route("/send/cashu", post(send_cashu))
        .route("/send/bolt11", post(send_bolt11))
        .route("/runway", get(runway))
        .route("/mints/list", get(mints_list))
        .route("/mints/info", get(mints_info))
        .route("/history", get(history))
        .route("/events", get(events))
        .route("/admin", get(admin_page))
        .route("/admin/mnemonic", get(admin_mnemonic));

    let router = public.merge(wallet_api).with_state(app);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".into());
    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("nutrail listening on 0.0.0.0:{port}");
    axum::serve(listener, router).await?;
    Ok(())
}
