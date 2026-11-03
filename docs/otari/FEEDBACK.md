# helide × Otari: feedback

## What did I build, and which features did I use?

helide-rs is my fork of Helix. I added three things that call open-weight models through Otari:

- **`:rewrite <instruction>`** rewrites every selection in parallel as one undo step. It uses the `helide-fast` policy: DeepSeek V4.1 Flash, falling back to Ministral 3 8B.
- **`:ask <question>`** answers about the selection in a popup that streams in. It uses `helide-deep`: Qwen3.5-397B, falling back to GLM-5.2. It has Otari's web search, and the code sandbox when the file is Python.
- **Commit messages in the git panel:** press `m`, and the staged diff, with secrets redacted, becomes a suggested message.

**Otari features used:**
- Routing policies with fallbacks.
- Two open-weight models in distinct roles.
- Hosted web search and code execution.
- The Lakera guardrail, in block mode.
- A $5/day budget. The editor shows live spend from `usage.cost_usd`.

Before anything leaves the editor, helide masks and redacts secrets: `.env` values, keys and tokens.

## What worked well?

- **It's OpenAI-compatible, so I needed no SDK.** Helix has no HTTP client, so I call Otari through `curl` with the key on stdin. Streaming over SSE with `include_usage` worked the first time.
- **Routing policies work as model names.** I send `model: "helide-fast"` and change the actual models in the dashboard without rebuilding the editor. That was the best part.
- **Response headers I could use right away.** `Otari-Model` and `Otari-Fallback` let the status line say who answered, and `usage.cost_usd` on every response let me show spend without a separate billing call.
- **Hosted tools are just tool names.** Adding `otari_web_search` to a request was enough, with no search API key of my own to manage.
- **Low overhead.** I measured about 0.2 s of gateway time. Slowness came from the model's output length, not from Otari.

## What didn't work?

- **The Lakera guardrail blocked harmless code.** With the default policy, plain requests like "fix the spelling in these comments" got flagged at a score of about 0.4. I ended up making a PII-only Lakera policy, with Enforce mode, L1, and the Name and IP detectors off. Repro: send code comments through a guardrailed route with Lakera's default policy.
- **The code sandbox is Python-only, and nothing tells the model.** When I asked to "run" a Rust function, the model returned convincing but invented "output" instead of failing. I now enable the sandbox only for Python files. A clear tool error for unsupported languages would prevent this.
- **Vague errors when a model can't serve a request.** Sending a chat request to `bge-m3`, an embedding model in the catalog, returns a 502 "No model could serve the request (2 attempts)" with no reason given. Meanwhile a model that isn't in the catalog gets a clear 400 `invalid_model`. It would help if the 502 said why.
- **Not every model I tried answered properly.** Kimi K2.6 returned an empty answer for the same prompt others answered, with no error or reason.

## What's missing, and what would I change?

- **Docs on when fallbacks trigger.** I couldn't find which status codes or timeouts make a routing policy move to the next model, so I had to work out how to force a fallback by trial and error.
- **Remaining budget in a response header.** I add up `cost_usd` myself to show spend. A header like `Otari-Budget-Remaining` would let clients warn before a request is refused.
- **Which languages the code sandbox supports,** in the model catalog or the tool definition, so clients can decide before sending.
- **A guardrail dry run:** a way to check a prompt against the guardrail without spending model tokens, for tuning policies.

## When was I most tempted to skip Otari and call the provider directly?

When Lakera kept blocking my own code comments. For a while, the guardrail was the only reason requests failed, and calling DeepSeek directly would have "fixed" it. Making a narrower Lakera policy was the right fix, but it took time to work out that the policy was the problem, not my prompts.

## What did I build myself that Otari should have done?

- **Secret redaction before sending.** Lakera checks for PII, but API keys, tokens and `.env` values were my job. A secrets-specific guardrail (key formats, high-entropy strings) would be a natural Otari feature.
- **A running spend total.** I add up the cost of every call myself (see "remaining budget" above).
- **Readable refusal messages.** I map guardrail and budget errors to plain sentences in the editor. Having that text in a consistent error field would help every client.

## If the rules hadn't required Otari, would I have kept using it?

Yes, for routing policies and the cost and model headers. Being able to swap the models behind `helide-fast` and `helide-deep` in the dashboard, without touching the editor, is the reason to keep it.

I'm less sure about the hosted tools. The sandbox is Python-only, which makes sense: a single Python file just runs. For an editor, though, a sandbox that can't run the language you're editing is only useful some of the time. A Rust sandbox is possible (the Rust Playground and Compiler Explorer do it), but what matters more is that the tool tells the model when a language isn't supported. Then it can't make up output.
