//! Otari: an OpenAI-compatible gateway to open-weight models, with each response's cost.
//!
//! Requests go through `curl`, so no HTTP client is linked in. The key is read from
//! `$OTARI_API_KEY` and handed to curl on its standard input, never on its command line, where
//! other processes could read it. What every answer cost is added up for the status line.

use std::{
    process::Stdio,
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;

const BASE_URL: &str = "https://eu.api.otari.ai/api/v1";

/// Fast and cheap, for short answers: commit messages, quick questions.
pub const SMALL_MODEL: &str = "mistralai/ministral-3-8b";
/// For rewrites and longer reasoning.
pub const LARGE_MODEL: &str = "openai/gpt-oss-120b";

/// What this session spent, in millionths of a dollar (Otari reports six decimals).
static SPENT_MICRO_USD: AtomicU64 = AtomicU64::new(0);

/// What this session spent so far, in dollars, once anything was spent.
pub fn spent() -> Option<f64> {
    let micro = SPENT_MICRO_USD.load(Ordering::Relaxed);
    (micro > 0).then(|| micro as f64 / 1_000_000.0)
}

/// An answer and what it cost.
pub struct Reply {
    pub text: String,
    pub cost_usd: f64,
}

/// Sends one chat request and returns the answer's text.
pub async fn chat(model: &str, messages: Value) -> anyhow::Result<Reply> {
    let key = std::env::var("OTARI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .context("OTARI_API_KEY is not set")?;
    let body = json!({ "model": model, "messages": messages }).to_string();
    let base = std::env::var("OTARI_BASE_URL").unwrap_or_else(|_| BASE_URL.to_string());

    let mut child = tokio::process::Command::new("curl")
        .args(["--silent", "--show-error", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not run curl")?;
    let config = format!(
        "url = \"{base}/chat/completions\"\n\
         header = \"Otari-Key: {}\"\n\
         header = \"Content-Type: application/json\"\n\
         data-binary = \"{}\"\n\
         max-time = 120\n\
         write-out = \"\\n%{{http_code}}\"\n",
        quote(&key),
        quote(&body),
    );
    let mut stdin = child.stdin.take().context("curl has no input")?;
    stdin.write_all(config.as_bytes()).await?;
    drop(stdin);

    let output = child.wait_with_output().await?;
    if !output.status.success() {
        bail!("curl: {}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let (response, status) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let response: Value = serde_json::from_str(response)
        .with_context(|| format!("Otari answered {status} with no JSON"))?;
    if status != "200" {
        let detail = response["detail"]
            .as_str()
            .or(response["error"]["message"].as_str())
            .unwrap_or("request failed");
        bail!("Otari {status}: {detail}");
    }

    let cost_usd = add_cost(&response);
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok(Reply { text, cost_usd })
}

/// Adds a response's `usage.cost_usd` to the session's spend, and returns it.
fn add_cost(response: &Value) -> f64 {
    let cost = response["usage"]["cost_usd"]
        .as_str()
        .and_then(|cost| cost.parse::<f64>().ok())
        .unwrap_or(0.0);
    SPENT_MICRO_USD.fetch_add((cost * 1_000_000.0).round() as u64, Ordering::Relaxed);
    cost
}

/// Escapes a value for a double-quoted string in a curl config file.
fn quote(value: &str) -> String {
    value.replace('\\', "\\\\").replace('"', "\\\"")
}
