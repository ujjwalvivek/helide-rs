//! The local model: an OpenAI-compatible server (llama.cpp on the datacenter box) that OpenCode
//! already knows about. helide reads the provider from OpenCode's config instead of keeping its
//! own, so a model added there is the one `:rewrite`, `:ask` and the git panel's `m` use too.
//!
//! The model is `$HELIDE_MODEL` (`provider/model`, as OpenCode names it), or else the first model
//! of the first provider in OpenCode's config that has a `baseURL`. Requests go through `curl`,
//! with the request (and any API key) on its standard input, never on its command line.

use std::{
    path::PathBuf,
    process::Stdio,
    sync::atomic::{AtomicU64, AtomicUsize, Ordering},
    time::Instant,
};

use anyhow::{bail, Context};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// The system prompt for rewriting a selection.
pub const REWRITE_PROMPT: &str = "Edit the code fragment as asked. It is part of a larger \
     file, so it may be incomplete; keep it that way and do not add or complete code. Reply with \
     the edited fragment only.";

/// The system prompt for a commit message.
pub const COMMIT_PROMPT: &str = "Write a one-line git commit message for the staged changes, \
     under 72 characters, in the style of the recent subjects. Reply with the message only.";

/// The system prompt for explaining code or answering a question about it.
pub const EXPLAIN_PROMPT: &str = "Answer the user's question about the code from their editor, \
     briefly, in Markdown.";

/// Added to [`EXPLAIN_PROMPT`] when web search is available.
pub const SEARCH_PROMPT: &str = " When the answer depends on documentation, versions or facts \
     that may have changed, search the web with web_search first, and link the pages you used.";

/// Requests on their way, for the status line's spinner.
static PENDING: AtomicUsize = AtomicUsize::new(0);
/// When the first of the requests now on their way was sent, in milliseconds since the epoch.
static SINCE_MS: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |time| time.as_millis() as u64)
}

/// While a request is on its way: a spinner frame and a phrase, both changing with time.
pub fn waiting() -> Option<String> {
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    const PHRASES: [&str; 12] = [
        "Thinking",
        "Pondering",
        "Noodling",
        "Ruminating",
        "Mulling",
        "Cogitating",
        "Percolating",
        "Hmming",
        "Brewing",
        "Untangling",
        "Scheming",
        "Whirring",
    ];
    if PENDING.load(Ordering::Relaxed) == 0 {
        return None;
    }
    let ms = now_ms();
    let frame = FRAMES[(ms / 80) as usize % FRAMES.len()];
    let phrase = PHRASES[(ms / 2500) as usize % PHRASES.len()];
    let waited = ms.saturating_sub(SINCE_MS.load(Ordering::Relaxed)) / 1000;
    Some(format!("{frame} {phrase}… ({waited}s)"))
}

/// Counts a request as on its way until dropped, and keeps the status line redrawing meanwhile.
struct Pending;

impl Pending {
    fn start() -> Self {
        if PENDING.fetch_add(1, Ordering::Relaxed) == 0 {
            SINCE_MS.store(now_ms(), Ordering::Relaxed);
            tokio::spawn(async {
                while PENDING.load(Ordering::Relaxed) > 0 {
                    helix_event::request_redraw();
                    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
                }
                helix_event::request_redraw();
            });
        }
        Pending
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Where requests go: one model of one OpenCode provider.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: Option<String>,
    /// The id the server knows the model by.
    pub model: String,
    /// What to call it in the status line: the provider's name for it, or its id.
    pub label: String,
}

/// OpenCode's config file: `$OPENCODE_CONFIG`, or `opencode.jsonc` / `opencode.json` in
/// `~/.config/opencode`.
fn opencode_config() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("OPENCODE_CONFIG") {
        return Some(path.into());
    }
    let dir = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))?
        .join("opencode");
    ["opencode.jsonc", "opencode.json"]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| path.is_file())
}

/// The endpoint to use, read from OpenCode's config each time, so edits there apply at once.
pub fn endpoint() -> anyhow::Result<Endpoint> {
    let path = opencode_config().context("no OpenCode config found (~/.config/opencode)")?;
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read {}", path.display()))?;
    let config: Value = serde_json::from_str(&strip_jsonc(&text))
        .with_context(|| format!("could not parse {}", path.display()))?;
    let wanted = std::env::var("HELIDE_MODEL").ok().filter(|m| !m.is_empty());
    find_endpoint(&config, wanted.as_deref())
}

fn find_endpoint(config: &Value, wanted: Option<&str>) -> anyhow::Result<Endpoint> {
    let providers = config["provider"]
        .as_object()
        .context("OpenCode's config has no providers with a baseURL")?;
    let (wanted_provider, wanted_model) = match wanted {
        Some(wanted) => {
            let (provider, model) = wanted
                .split_once('/')
                .context("HELIDE_MODEL should be provider/model, as OpenCode names it")?;
            (Some(provider), Some(model))
        }
        None => (None, None),
    };
    for (id, provider) in providers {
        if wanted_provider.is_some_and(|wanted| wanted != id) {
            continue;
        }
        let Some(base_url) = provider["options"]["baseURL"].as_str() else {
            continue;
        };
        let Some(models) = provider["models"].as_object() else {
            continue;
        };
        let found = match wanted_model {
            Some(wanted) => models.get_key_value(wanted),
            None => models.iter().next(),
        };
        let Some((model, info)) = found else {
            continue;
        };
        let api_key = provider["options"]["apiKey"]
            .as_str()
            .map(expand_env)
            .filter(|key| !key.is_empty());
        return Ok(Endpoint {
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model: model.clone(),
            label: info["name"].as_str().unwrap_or(model).to_string(),
        });
    }
    match wanted {
        Some(wanted) => bail!("{wanted} is not an OpenCode provider model with a baseURL"),
        None => bail!("OpenCode's config has no providers with a baseURL"),
    }
}

/// OpenCode's `{env:NAME}` substitution, for API keys kept out of the file.
fn expand_env(value: &str) -> String {
    match value
        .strip_prefix("{env:")
        .and_then(|rest| rest.strip_suffix('}'))
    {
        Some(name) => std::env::var(name).unwrap_or_default(),
        None => value.to_string(),
    }
}

/// JSON with comments and trailing commas, as OpenCode accepts it, turned into plain JSON.
fn strip_jsonc(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if in_string {
            out.push(c);
            match c {
                '\\' => out.extend(chars.next()),
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match (c, chars.peek()) {
            ('"', _) => {
                in_string = true;
                out.push(c);
            }
            ('/', Some('/')) => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            ('/', Some('*')) => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
            }
            (',', _) => {
                // A comma followed only by whitespace before a closing bracket is dropped.
                let rest: String = chars.clone().take_while(|c| c.is_whitespace()).collect();
                let next = chars.clone().nth(rest.chars().count());
                if !matches!(next, Some('}' | ']')) {
                    out.push(c);
                }
            }
            _ => out.push(c),
        }
    }
    out
}

/// An answer, who gave it and how long it took.
pub struct Reply {
    pub text: String,
    pub label: String,
    pub seconds: f64,
    /// Tool calls the model made instead of (or before) answering, in the API's shape.
    tool_calls: Vec<Value>,
}

impl Reply {
    /// For the status line: `Qwen3.6 35B-A3B in 1.2s`.
    pub fn by(&self) -> String {
        format!("{} in {:.1}s", self.label, self.seconds)
    }
}

/// Sends one chat request and returns the answer. `think` lets a reasoning model reason first,
/// which is slower and better for questions than for quick edits.
pub async fn chat(messages: Value, think: bool) -> anyhow::Result<Reply> {
    chat_with(messages, think, None, None).await
}

/// Like [`chat`], but the answer streams in: `on_text` gets the text so far as it arrives, at most
/// every 100 milliseconds, and once more when it is complete.
pub async fn chat_streaming(
    messages: Value,
    think: bool,
    on_text: impl Fn(&str) + Send + Sync,
) -> anyhow::Result<Reply> {
    chat_with(messages, think, Some(&on_text), None).await
}

/// The SearXNG instance to search, from `$SEARXNG_URL`, if one is set.
pub fn search_url() -> Option<String> {
    std::env::var("SEARXNG_URL")
        .ok()
        .filter(|url| !url.is_empty())
        .map(|url| url.trim_end_matches('/').to_string())
}

/// Like [`chat_streaming`] with reasoning on, but the model may search the web first, through
/// the SearXNG at `$SEARXNG_URL`. While it searches, `on_text` shows what it is looking for.
pub async fn ask_streaming(
    messages: Value,
    on_text: impl Fn(&str) + Send + Sync,
) -> anyhow::Result<Reply> {
    /// Searches allowed before the model has to answer with what it has.
    const ROUNDS: usize = 3;

    let Some(search) = search_url() else {
        return chat_with(messages, true, Some(&on_text), None).await;
    };
    let tools = json!([{
        "type": "function",
        "function": {
            "name": "web_search",
            "description": "Search the web. Returns titles, URLs and snippets.",
            "parameters": {
                "type": "object",
                "properties": { "query": { "type": "string", "description": "What to search for" } },
                "required": ["query"],
            },
        },
    }]);
    let started = Instant::now();
    let mut messages = messages;
    for round in 0..=ROUNDS {
        let tools = (round < ROUNDS).then_some(&tools);
        let mut reply = chat_with(messages.clone(), true, Some(&on_text), tools).await?;
        if reply.tool_calls.is_empty() {
            reply.seconds = started.elapsed().as_secs_f64();
            return Ok(reply);
        }
        let list = messages.as_array_mut().context("messages should be a list")?;
        list.push(json!({ "role": "assistant", "content": reply.text, "tool_calls": reply.tool_calls }));
        for call in &reply.tool_calls {
            let arguments: Value =
                serde_json::from_str(call["function"]["arguments"].as_str().unwrap_or("{}"))
                    .unwrap_or_default();
            let query = arguments["query"].as_str().unwrap_or_default().to_string();
            on_text(&format!("*Searching the web for “{query}”…*"));
            let result = match web_search(&search, &query).await {
                Ok(result) => result,
                Err(err) => format!("Search failed: {err}"),
            };
            list.push(json!({ "role": "tool", "tool_call_id": call["id"], "content": result }));
        }
    }
    bail!("the model kept searching without answering")
}

/// One SearXNG search, as numbered results with title, URL and snippet.
async fn web_search(base: &str, query: &str) -> anyhow::Result<String> {
    let output = tokio::process::Command::new("curl")
        .args(["--silent", "--show-error", "--fail", "--max-time", "20", "--get"])
        .args(["--data-urlencode", &format!("q={query}"), "--data", "format=json"])
        .arg(format!("{base}/search"))
        .kill_on_drop(true)
        .output()
        .await
        .context("could not run curl")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    let data: Value = serde_json::from_slice(&output.stdout).context("search gave no JSON")?;
    let results: Vec<String> = data["results"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .take(8)
        .enumerate()
        .map(|(i, result)| {
            let snippet: String = result["content"]
                .as_str()
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(300)
                .collect();
            format!(
                "{}. {}\n   {}\n   {snippet}",
                i + 1,
                result["title"].as_str().unwrap_or_default(),
                result["url"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    if results.is_empty() {
        return Ok(format!("No results for \"{query}\"."));
    }
    Ok(results.join("\n\n"))
}

async fn chat_with(
    messages: Value,
    think: bool,
    on_text: Option<&(dyn Fn(&str) + Send + Sync)>,
    tools: Option<&Value>,
) -> anyhow::Result<Reply> {
    let endpoint = endpoint()?;
    let _pending = Pending::start();
    let started = Instant::now();
    let mut body = json!({
        "model": endpoint.model,
        "messages": messages,
        // llama.cpp passes this to the chat template; Qwen's turns reasoning on or off with it.
        "chat_template_kwargs": { "enable_thinking": think },
    });
    if on_text.is_some() {
        body["stream"] = true.into();
    }
    if let Some(tools) = tools {
        body["tools"] = tools.clone();
    }
    let mut config = format!(
        "url = \"{}/chat/completions\"\n\
         header = \"Content-Type: application/json\"\n\
         data-binary = \"{}\"\n\
         connect-timeout = 5\n\
         max-time = 600\n\
         no-buffer\n\
         write-out = \"\\n%{{http_code}}\"\n",
        endpoint.base_url,
        quote(&body.to_string()),
    );
    if let Some(key) = &endpoint.api_key {
        config.push_str(&format!("header = \"Authorization: Bearer {}\"\n", quote(key)));
    }

    let mut child = tokio::process::Command::new("curl")
        .args(["--silent", "--show-error", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("could not run curl")?;
    let mut stdin = child.stdin.take().context("curl has no input")?;
    stdin.write_all(config.as_bytes()).await?;
    drop(stdin);

    // A streamed answer comes as `data: {chunk}` lines. Everything else, a whole answer, an error,
    // and the status code curl writes last, is kept for after.
    let mut lines =
        tokio::io::BufReader::new(child.stdout.take().context("curl has no output")?).lines();
    let (mut answer, mut rest) = (String::new(), String::new());
    // Streamed tool calls arrive in pieces, each piece naming the call's index.
    let mut calls: Vec<Value> = Vec::new();
    let mut shown = Instant::now();
    while let Some(line) = lines.next_line().await? {
        match (line.strip_prefix("data: "), on_text) {
            (Some("[DONE]"), Some(_)) => {}
            (Some(data), Some(on_text)) => {
                let Ok(chunk) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                let delta = &chunk["choices"][0]["delta"];
                if let Some(text) = delta["content"].as_str() {
                    answer.push_str(text);
                }
                for piece in delta["tool_calls"].as_array().map(Vec::as_slice).unwrap_or_default() {
                    let index = piece["index"].as_u64().unwrap_or(0) as usize;
                    while calls.len() <= index {
                        calls.push(json!({ "id": "", "type": "function", "function": { "name": "", "arguments": "" } }));
                    }
                    let call = &mut calls[index];
                    if let Some(id) = piece["id"].as_str() {
                        call["id"] = id.into();
                    }
                    for field in ["name", "arguments"] {
                        if let Some(part) = piece["function"][field].as_str() {
                            let joined = format!("{}{part}", call["function"][field].as_str().unwrap_or_default());
                            call["function"][field] = joined.into();
                        }
                    }
                }
                let text = answer.trim_start_matches(['\n', '\r']);
                if !text.is_empty() && shown.elapsed() >= std::time::Duration::from_millis(100) {
                    on_text(text);
                    shown = Instant::now();
                }
            }
            _ => {
                rest.push_str(&line);
                rest.push('\n');
            }
        }
    }
    let output = child.wait_with_output().await?;
    if !output.status.success() {
        bail!(
            "{} at {}: {}",
            endpoint.label,
            endpoint.base_url,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let stdout = rest.trim_end_matches('\n');
    let (response, status) = stdout.rsplit_once('\n').unwrap_or(("", stdout));
    let text = match on_text {
        Some(on_text) if status == "200" => {
            if calls.is_empty() {
                on_text(answer.trim_start_matches(['\n', '\r']));
            }
            answer
        }
        _ => {
            let response: Value = serde_json::from_str(response)
                .with_context(|| format!("{} answered {status} with no JSON", endpoint.label))?;
            if status != "200" {
                let message = response["error"]["message"]
                    .as_str()
                    .unwrap_or("request failed");
                bail!("{} {status}: {message}", endpoint.label);
            }
            if let Some(list) = response["choices"][0]["message"]["tool_calls"].as_array() {
                calls = list.clone();
            }
            response["choices"][0]["message"]["content"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        }
    };
    Ok(Reply {
        text: text
            .trim_end()
            .trim_start_matches(['\n', '\r'])
            .to_string(),
        label: endpoint.label,
        seconds: started.elapsed().as_secs_f64(),
        tool_calls: calls,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"// comment
{
  "$schema": "https://opencode.ai/config.json", // trailing
  /* block */
  "provider": {
    "cloud": { "options": {}, "models": { "x": {} } },
    "datacenter": {
      "options": { "baseURL": "http://host:8081/v1/" },
      "models": {
        "qwen3.6": { "name": "Qwen3.6 35B-A3B", },
        "small": {},
      },
    },
  },
}"#;

    #[test]
    fn reads_opencode_providers() {
        let config: Value = serde_json::from_str(&strip_jsonc(CONFIG)).unwrap();
        let first = find_endpoint(&config, None).unwrap();
        assert_eq!(first.base_url, "http://host:8081/v1");
        assert_eq!(first.model, "qwen3.6");
        assert_eq!(first.label, "Qwen3.6 35B-A3B");
        let named = find_endpoint(&config, Some("datacenter/small")).unwrap();
        assert_eq!(named.label, "small");
        assert!(find_endpoint(&config, Some("cloud/x")).is_err());
    }

    #[test]
    fn keeps_slashes_in_strings() {
        let text = r#"{ "url": "http://a//b", "s": "/* no */" }"#;
        let value: Value = serde_json::from_str(&strip_jsonc(text)).unwrap();
        assert_eq!(value["url"], "http://a//b");
        assert_eq!(value["s"], "/* no */");
    }
}
