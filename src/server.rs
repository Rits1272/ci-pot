//! The HTTP side: where funders pay and anyone can read a pot.
//!
//! | Request                  | Answer                                                    |
//! |--------------------------|-----------------------------------------------------------|
//! | `GET /`                  | a page listing the price and every pot                    |
//! | `GET /info`              | the coordinator, price, accepted mints and minimum balance |
//! | `GET /pots`              | every pot                                                 |
//! | `GET /pots/{address}`    | one pot with its recent history                           |
//! | `POST /pots/{address}`   | pay a Cashu token into that repository's pot              |
//!
//! A refused payment answers `400` (bad address or token), `409` (token
//! already paid in), `402` (the mint refused it) or `500` (a fault here).
//!
//! `{address}` is a repository address, `30617:<pubkey>:<name>`. Reading is
//! open to everyone on purpose: funders should be able to see where their
//! sats went.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use nostr::prelude::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::job::{name_of, owner_of};
use crate::pot::{now, PotView, Refused, Service};

pub fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/", get(page))
        .route("/info", get(info))
        .route("/pots", get(pots))
        .route("/pots/{address}", get(pot).post(deposit))
        .with_state(service)
}

async fn info(State(service): State<Arc<Service>>) -> Json<Value> {
    let config = &service.config;
    Json(json!({
        "name": "ci-pot",
        "version": env!("CARGO_PKG_VERSION"),
        "coordinator": npub(&config.coordinator),
        "sat_per_vcpu_hour": config.price.msat_per_vcpu_hour / 1000,
        "default_vcpus": config.price.default_vcpus,
        "min_balance_sat": config.min_balance_msat / 1000,
        "mints": service.cashier.mints(),
    }))
}

async fn pots(State(service): State<Arc<Service>>) -> Response {
    match service.pots() {
        Ok(pots) => Json(pots).into_response(),
        Err(e) => error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

async fn pot(State(service): State<Arc<Service>>, Path(address): Path<String>) -> Response {
    let found = match service.pot(&address) {
        Ok(Some(found)) => found,
        Ok(None) => return error(StatusCode::NOT_FOUND, "no pot for this repository yet"),
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let history = service.entries(found.pot.id).unwrap_or_default();
    Json(json!({ "pot": found, "history": history })).into_response()
}

#[derive(Deserialize)]
struct Payment {
    /// A Cashu token, `cashuA...` or `cashuB...`.
    token: String,
}

async fn deposit(
    State(service): State<Arc<Service>>,
    Path(address): Path<String>,
    Json(payment): Json<Payment>,
) -> Response {
    match service.deposit(&address, &payment.token, now()).await {
        Ok(pot) => Json(json!({ "pot": pot })).into_response(),
        Err(Refused::Invalid(reason)) => error(StatusCode::BAD_REQUEST, &reason),
        Err(Refused::AlreadyUsed) => error(StatusCode::CONFLICT, "this token was already paid in"),
        Err(Refused::Mint(reason)) => error(StatusCode::PAYMENT_REQUIRED, &reason),
        Err(Refused::Internal(reason)) => error(StatusCode::INTERNAL_SERVER_ERROR, &reason),
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

async fn page(State(service): State<Arc<Service>>) -> Response {
    let pots = match service.pots() {
        Ok(pots) => pots,
        Err(e) => return error(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let config = &service.config;
    let mut rows = String::new();
    for view in &pots {
        rows.push_str(&row(view));
    }
    if pots.is_empty() {
        rows.push_str("<tr><td colspan=\"5\">No repository has been funded yet.</td></tr>");
    }
    let mints = service
        .cashier
        .mints()
        .iter()
        .map(|mint| format!("<li><code>{}</code></li>", escape(mint)))
        .collect::<String>();

    Html(
        PAGE.replace("{{coordinator}}", &escape(&npub(&config.coordinator)))
            .replace(
                "{{price}}",
                &(config.price.msat_per_vcpu_hour / 1000).to_string(),
            )
            .replace("{{default_vcpus}}", &config.price.default_vcpus.to_string())
            .replace(
                "{{min_balance}}",
                &(config.min_balance_msat / 1000).to_string(),
            )
            .replace("{{mints}}", &mints)
            .replace("{{rows}}", &rows),
    )
    .into_response()
}

fn row(view: &PotView) -> String {
    let address = &view.pot.address;
    format!(
        "<tr><td><strong>{}</strong><span class=\"who\">{}</span><code class=\"address\">{}</code></td>\
         <td>{}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
        escape(name_of(address)),
        escape(&npub(owner_of(address))),
        escape(address),
        sat(view.balance_msat),
        sat(view.pot.charged_msat as i64),
        view.pot.jobs,
        if view.admitted { "yes" } else { "no" },
    )
}

/// Millisatoshis as whole sats, rounded toward zero.
fn sat(msat: i64) -> String {
    (msat / 1000).to_string()
}

fn npub(hex: &str) -> String {
    PublicKey::from_hex(hex)
        .ok()
        .and_then(|key| key.to_bech32().ok())
        .unwrap_or_else(|| hex.to_string())
}

/// Repository names come from strangers, so they are escaped before going into HTML.
fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>CI pots</title>
<style>
  :root { --paper:#f3f5f7; --card:#ffffff; --ink:#15202b; --soft:#5b6976; --line:#d5dce3; --mark:#0b6e99; }
  @media (prefers-color-scheme: dark) {
    :root { --paper:#0f151b; --card:#161e26; --ink:#e1e8ee; --soft:#93a1ae; --line:#2a3642; --mark:#5bb8dd; }
  }
  * { box-sizing: border-box; }
  body { margin:0; background:var(--paper); color:var(--ink); font:16px/1.5 system-ui, sans-serif; }
  main { max-width: 56rem; margin: 0 auto; padding: 2.5rem 1rem 4rem; }
  h1 { font-size: 1.9rem; line-height: 1.2; margin: 0 0 .4rem; }
  h2 { font-size: 1.15rem; margin: 2.5rem 0 .6rem; }
  p, li { max-width: 42rem; }
  .soft, .who { color: var(--soft); }
  .scroll, form { background: var(--card); border: 1px solid var(--line); border-radius: 6px; }
  .scroll { overflow-x: auto; }
  table { border-collapse: collapse; width: 100%; font-variant-numeric: tabular-nums; }
  th, td { padding: .55rem .8rem; text-align: right; border-bottom: 1px solid var(--line); white-space: nowrap; }
  th:first-child, td:first-child { text-align: left; white-space: normal; }
  tr:last-child td { border-bottom: 0; }
  th { font-weight: 600; font-size: .85rem; color: var(--soft); }
  .who, .address { display:block; font-size: .78rem; }
  .address { color: var(--soft); word-break: break-all; }
  code { font: .85rem ui-monospace, monospace; }
  form { padding: 1rem 1.2rem; display: grid; gap: .7rem; }
  label { font-weight: 600; font-size: .9rem; }
  input, textarea { width: 100%; padding: .5rem .6rem; font: .9rem ui-monospace, monospace;
    color: var(--ink); background: var(--paper); border: 1px solid var(--line); border-radius: 4px; }
  button { justify-self: start; padding: .55rem 1.1rem; font: inherit; font-weight: 600; cursor: pointer;
    color: #fff; background: var(--mark); border: 0; border-radius: 4px; }
  @media (prefers-color-scheme: dark) { button { color: #0f151b; } }
  :focus-visible { outline: 2px solid var(--mark); outline-offset: 2px; }
  #result { min-height: 1.5rem; margin: 0; }
</style>
</head>
<body>
<main>
  <h1>CI pots</h1>
  <p class="soft">Coordinator <code>{{coordinator}}</code></p>
  <p>Anyone can pay sats into a repository's pot. This coordinator runs CI for a repository while its pot holds more than {{min_balance}} sat, and takes the cost of each job out of the pot.</p>

  <h2>Price</h2>
  <p><strong>{{price}} sat per vCPU-hour.</strong> A job costs its running time, times its vCPUs, times that rate. A job that does not state its vCPUs is charged for {{default_vcpus}}.</p>

  <h2>Pots</h2>
  <div class="scroll"><table>
    <tr><th>Repository</th><th>Balance (sat)</th><th>Spent (sat)</th><th>Jobs</th><th>Served now</th></tr>
    {{rows}}
  </table></div>

  <h2>Fund a repository</h2>
  <p>Paste the repository's address and a Cashu token from one of the accepted mints. The whole token is credited. Pots are not refundable.</p>
  <ul>{{mints}}</ul>
  <form id="fund">
    <label for="address">Repository address</label>
    <input id="address" required placeholder="30617:<maintainer public key in hex>:<repository name>">
    <label for="token">Cashu token</label>
    <textarea id="token" rows="3" required placeholder="cashuB..."></textarea>
    <button>Pay into pot</button>
    <p id="result" role="status"></p>
  </form>

  <h2>Before you pay</h2>
  <ul>
    <li>The operator of this coordinator holds the money. Fund only what you would trust them with.</li>
    <li>A pot can end slightly below zero, because jobs already running when it empties are still charged.</li>
    <li>A maintainer must also ask this coordinator for service (a Service Request) before it runs anything.</li>
  </ul>
</main>
<script>
document.getElementById('fund').addEventListener('submit', async (event) => {
  event.preventDefault();
  const result = document.getElementById('result');
  const address = document.getElementById('address').value.trim();
  const token = document.getElementById('token').value.trim();
  result.textContent = 'Paying in...';
  try {
    const response = await fetch('/pots/' + encodeURIComponent(address), {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({ token }),
    });
    const body = await response.json();
    result.textContent = response.ok
      ? 'Paid in. Balance is now ' + Math.floor(body.pot.balance_msat / 1000) + ' sat. Reload to see it in the table.'
      : 'Not accepted: ' + body.error;
  } catch (e) {
    result.textContent = 'Could not reach the server.';
  }
});
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pot::tests::{address, fund, service, NOW};

    async fn start(dir: &std::path::Path) -> (String, Arc<Service>) {
        let (service, _) = service(dir, vec![]).await;
        let service = Arc::new(service);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = router(service.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, service)
    }

    /// A repository address as it must appear in a URL path.
    fn in_path(address: &str) -> String {
        address.replace(':', "%3A")
    }

    #[tokio::test]
    async fn info_states_the_price_and_the_mints() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = start(dir.path()).await;
        let body: Value = reqwest::get(format!("{url}/info"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(body["sat_per_vcpu_hour"], 60);
        assert_eq!(body["default_vcpus"], 4);
        assert_eq!(body["mints"][0], "https://mint.example");
        assert!(body["coordinator"].as_str().unwrap().starts_with("npub1"));
    }

    #[tokio::test]
    async fn a_funded_pot_can_be_read_by_anyone() {
        let dir = tempfile::tempdir().unwrap();
        let (url, service) = start(dir.path()).await;
        let repo = address(&Keys::generate(), "app");
        fund(&service, &repo, 250, NOW);

        let one: Value = reqwest::get(format!("{url}/pots/{}", in_path(&repo)))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(one["pot"]["balance_msat"], 250_000);
        assert_eq!(one["pot"]["admitted"], true);
        assert_eq!(one["history"][0]["kind"], "deposit");

        let all: Value = reqwest::get(format!("{url}/pots"))
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(all.as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_unfunded_repository_has_no_pot() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = start(dir.path()).await;
        let repo = address(&Keys::generate(), "app");
        let response = reqwest::get(format!("{url}/pots/{}", in_path(&repo)))
            .await
            .unwrap();
        assert_eq!(response.status(), 404);
    }

    #[tokio::test]
    async fn paying_with_something_that_is_not_a_token_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let (url, service) = start(dir.path()).await;
        let repo = address(&Keys::generate(), "app");
        let response = reqwest::Client::new()
            .post(format!("{url}/pots/{}", in_path(&repo)))
            .json(&json!({ "token": "hello" }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        assert!(service.pots().unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_page_escapes_repository_names() {
        let dir = tempfile::tempdir().unwrap();
        let (url, service) = start(dir.path()).await;
        let repo = address(&Keys::generate(), "<img src=x onerror=alert(1)>");
        fund(&service, &repo, 10, NOW);
        let page = reqwest::get(&url).await.unwrap().text().await.unwrap();
        assert!(!page.contains("<img src=x"));
        assert!(page.contains("&lt;img src=x"));
        assert!(!page.contains("{{"));
    }
}
