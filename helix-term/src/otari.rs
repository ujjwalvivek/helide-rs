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

// Both are Otari routing policies, set up in the workspace's Routing page: each tries its main
// model and falls back to the next when that fails. Otari names the model that answered in the
// response, so the status line can say which one it was.

/// Fast and cheap, for rewrites and commit messages: `deepseek/deepseek-v4.1-flash`, falling
/// back to `mistralai/ministral-3-8b`.
pub const SMALL_MODEL: &str = "helide-fast";
/// For explanations, with web search: `alibaba/qwen-3.5-397b-a17b`, falling back to
/// `z-ai/glm-5.2`.
pub const LARGE_MODEL: &str = "helide-deep";

/// The system prompt for rewriting a selection.
///
/// Kept short and plain: longer prompts full of "follow the instruction" read as prompt injection
/// to guardrails such as Lakera's, which then block the request.
pub const REWRITE_PROMPT: &str = "Edit the code fragment as asked. It is part of a larger \
     file, so it may be incomplete; keep it that way and do not add or complete code. Reply with \
     the edited fragment only.";

/// The system prompt for a commit message.
pub const COMMIT_PROMPT: &str = "Write a one-line git commit message for the staged changes, \
     under 72 characters, in the style of the recent subjects. Reply with the message only.";

/// The system prompt for explaining code or answering a question about it.
pub const EXPLAIN_PROMPT: &str = "Answer the user's question about the code from their editor, \
     briefly, in Markdown. Search the web for the relevant official documentation before \
     answering, and link the pages you used.";

/// Added to [`EXPLAIN_PROMPT`] for Python, the one language Otari's code sandbox runs.
///
/// Other languages get no sandbox: asked to run Rust, a model reported a made-up panic as the
/// "real output" instead of the NaN the code returns.
pub const RUN_PROMPT: &str = " When running the code would show what it does or confirm your \
     answer, run it in the code sandbox and include the real output.";

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
    /// The model that answered, without its vendor.
    pub model: String,
    /// Whether a routing policy's main model failed and a backup answered.
    pub fell_back: bool,
}

impl Reply {
    /// Who answered, for the status line: `ministral-3-8b`, or `deepseek-v3.2 (fallback)`.
    pub fn by(&self) -> String {
        if self.fell_back {
            format!("{} (fallback)", self.model)
        } else {
            self.model.clone()
        }
    }
}

/// Sends one chat request and returns the answer's text. `tools` are Otari's hosted tools, such
/// as `otari_web_search`, which run on Otari's side within the request.
pub async fn chat(model: &str, messages: Value, tools: &[&str]) -> anyhow::Result<Reply> {
    let model_asked = model;
    let key = std::env::var("OTARI_API_KEY")
        .ok()
        .filter(|key| !key.is_empty())
        .context("OTARI_API_KEY is not set")?;
    let mut body = json!({ "model": model, "messages": &messages });
    if !tools.is_empty() {
        let tools: Vec<Value> = tools.iter().map(|tool| json!({ "type": tool })).collect();
        body["tools"] = tools.into();
    }
    let body = body.to_string();
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
         write-out = \"\\n%{{http_code}}\\t%header{{otari-model}}\\t%header{{otari-fallback}}\\t%header{{otari-request-id}}\"\n",
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
    let (response, trailer) = stdout.rsplit_once('\n').unwrap_or(("", &stdout));
    let mut trailer = trailer.split('\t');
    let status = trailer.next().unwrap_or_default();
    let model = trailer.next().unwrap_or_default();
    let model = model.rsplit('/').next().unwrap_or(model).to_string();
    let fell_back = trailer.next() == Some("true");
    let request_id = trailer.next().unwrap_or_default();
    let response: Value = serde_json::from_str(response)
        .with_context(|| format!("Otari answered {status} with no JSON"))?;
    log(json!({
        "time": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |time| time.as_secs()),
        "policy": model_asked,
        "tools": tools,
        "status": status,
        "request_id": request_id,
        "answered_by": &model,
        "fell_back": fell_back,
        "cost_usd": response["usage"]["cost_usd"],
        "messages": messages,
        "reply": response["choices"][0]["message"]["content"],
        "refusal": response["detail"],
    }));
    if status != "200" {
        bail!("{}", refusal(status, &response));
    }

    let cost_usd = add_cost(&response);
    let text = response["choices"][0]["message"]["content"]
        .as_str()
        .unwrap_or_default()
        .trim_end()
        .trim_start_matches(['\n', '\r'])
        .to_string();
    Ok(Reply {
        text,
        cost_usd,
        model,
        fell_back,
    })
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

/// The text inside a reply wrapped in one code fence, which models add despite being asked not to.
pub fn strip_fences(text: &str) -> &str {
    let Some(rest) = text.strip_prefix("```") else {
        return text;
    };
    let Some((tag, body)) = rest.split_once('\n') else {
        return text;
    };
    // A language tag after the fence, or code that started on the fence's own line.
    let body = if tag
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+-_#.".contains(c))
    {
        body
    } else {
        rest
    };
    body.trim_end()
        .strip_suffix("```")
        .unwrap_or(body)
        .trim_end_matches('\n')
}

/// Why Otari refused a request, in words: a guardrail, the budget, or its own message.
fn refusal(status: &str, response: &Value) -> String {
    let detail = &response["detail"];
    match detail["code"].as_str() {
        Some("guardrail_violation") => {
            let guardrail = detail["guardrails"][0]["profile"]
                .as_str()
                .unwrap_or("a guardrail");
            format!("Blocked by Otari's {guardrail} guardrail, so nothing was sent to the model")
        }
        Some("budget_exceeded") => "Otari budget used up until it resets".to_string(),
        _ => {
            let message = detail
                .as_str()
                .or(detail["message"].as_str())
                .or(response["error"]["message"].as_str())
                .unwrap_or("request failed");
            format!("Otari {status}: {message}")
        }
    }
}

/// Appends one exchange, as a line of JSON, to the file `$HELIDE_OTARI_LOG` names, if it is set.
/// The messages are what was sent, so secrets in them were already redacted.
fn log(entry: Value) {
    use std::io::Write;

    let Some(path) = std::env::var_os("HELIDE_OTARI_LOG") else {
        return;
    };
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path);
    if let Ok(mut file) = file {
        let _ = writeln!(file, "{entry}");
    }
}
