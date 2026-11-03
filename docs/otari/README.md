# helide-rs × Otari

helide-rs is a fork of Helix with an editor-native layer of AI commands. Every request goes through
[Otari](https://otari.ai) to open-weight models. This folder holds the results of real calls helide made,
recorded by helide itself, for the Mozilla Developers Wilding hackathon.

`calls.jsonl` has one line per request, written by helide when `HELIDE_OTARI_LOG` names a file:
the routing policy asked, the model that answered, whether it was a fallback, the cost, Otari's
request id, the messages exactly as sent (secrets already redacted) and the reply or refusal. The
request ids match Otari's own usage records.

The calls were made in a small demo repository (`stats.rs`, `stats.py`, a `.env` and a `.gitignore`)
by driving helide through its own keys and commands. The values in the demo `.env` and the email
address `jane.doe@example.com` are made up.

## What helide does with Otari

| Command | What it does | Policy |
|---|---|---|
| `:rewrite <instruction>` | Rewrites every selection as instructed, in parallel, in place, as one undo step. Selections holding a secret are not sent. | `helide-fast` |
| `:ask [question]` (or `:explain`) | Answers a question about the selection, or the cursor's line, in a popup, streaming it in as it is written. Searches the web for documentation, and for Python runs the code in Otari's sandbox. | `helide-deep` |
| `m` in the git panel | Writes a one-line commit message for the staged changes, in the style of the recent subjects, and opens the commit prompt with it. | `helide-fast` |

The status line shows what the session has spent, and every answer names the model that gave it.

## Otari features used

| Feature | How helide uses it | Calls |
|---|---|---|
| Open-weight models | DeepSeek V4.1 Flash for edits and commit messages, Qwen 3.5 397B for explanations, both open-weight on Hugging Face | all |
| Multiple models, distinct roles | a fast model for small edits, a large one that searches and runs code for explanations | 1–3, 5, 8 vs 6, 7, 9 |
| Routing and fallbacks | helide only names the policies `helide-fast` (DeepSeek V4.1 Flash, then Ministral 3 8B) and `helide-deep` (Qwen 3.5 397B, then GLM-5.2). Otari picks the model, falls back on failure, and reports which model answered in its `Otari-Model` and `Otari-Fallback` headers, which helide shows | all |
| Web search | `:ask` declares `otari_web_search` and asks for documentation, which the answer links | 6, 9 |
| Code execution | `:ask` on Python declares `otari_code_execution`; the model ran the code and quoted the real result | 9 |
| Guardrails | Lakera Guard mandated in Block mode with a PII-only policy, so personal data never reaches a model; helide says which guardrail refused | 4 |
| Budgets and spending limits | a $5/day spend ceiling on the workspace; helide adds up each response's `usage.cost_usd` in the status line, and turns a `budget_exceeded` refusal into a plain message (not triggered in these calls) | all |

Before anything reaches Otari, helide applies its own check: values in `.env` and key files, and
well-known token shapes (AWS, GitHub, Stripe, private keys, JWTs…), are replaced with `[REDACTED]`
in what `:ask` sends; `:rewrite` refuses to send a selection that holds one; and the commit message
names staged `.env` and key files without their contents and redacts tokens in the rest of the diff.
The same rules mask `.env` values on screen and keep them out of the selection the Claude Code and MCP
integrations see.

## The calls

(The calls were recorded under the commands' earlier names, `:ask` for rewriting and `:explain` for
questions; they are given here under the current ones.)

1–3. **`:rewrite fix the spelling` on three comments at once** (selected with `%` and `s`). Three requests
in parallel to `helide-fast`, each answered by DeepSeek V4.1 Flash for about $0.00002:
`// add up all teh numbers in the list and retrun it` became
`// add up all the numbers in the list and return it`, and so on.

4. **The guardrail.** `:rewrite make this comment clearer` on
`// ask jane.doe@example.com for the real readings`. Otari's Lakera guardrail blocked it before any
model saw it (`403 guardrail_violation`, score 1.0), and helide reported
"Blocked by Otari's lakera-guard guardrail, so nothing was sent to the model".

5. **`:rewrite return an Option<i32> instead of panicking on an empty slice`** on `largest`:

```rust
fn largest(xs: &[i32]) -> Option<i32> {
    // find the biggest reading
    let mut best = *xs.first()?;
    for &x in xs {
        if x > best { best = x; }
    }
    Some(best)
}
```

6. **`:ask is there a more idiomatic way to write this? link the docs`** on `total`. Qwen 3.5 397B
searched and linked `Iterator::sum` and the `Sum` trait ($0.0129). Its first suggestion, `xs.iter().sum()`,
is right; its second, `xs.sum()`, does not compile. The answers are the models', unedited.

7. **`:ask` on Python with an empty code block.** A timing slip in the script that drove helide
selected a blank line instead of the function, and the model asked for the code. Kept for completeness.

8. **`m` in the git panel** with the edits from 1–5 staged:
`stats: fix typos and make largest return Option`.

9. **`:ask is this correct for an even number of readings? run it to check`** on the Python `median`.
Qwen 3.5 397B ran the function in Otari's sandbox and searched the docs ($0.0243): it returns `3` for
`[1, 2, 3, 4]` where the median is `2.5`, and it gave the corrected function with a link to
`statistics.median`.

No call in this set needed a fallback (`"fell_back": false` throughout).

## Trying it

```sh
cargo build
export OTARI_API_KEY=otk_...            # a workspace key
export HELIDE_OTARI_LOG=otari.jsonl     # optional: record every call
./target/debug/hx some-file.rs
```

The workspace needs the two routing policies above, web search and code execution allowed under
Tools, and optionally a guardrail and a spend ceiling. `OTARI_BASE_URL` overrides the EU endpoint,
`https://eu.api.otari.ai/api/v1`. Requests go through `curl`, with the key passed on its standard input.

Feedback on building this with Otari, including what worked and what didn't: [FEEDBACK.md](./FEEDBACK.md).
