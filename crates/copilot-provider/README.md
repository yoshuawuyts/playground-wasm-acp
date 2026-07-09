# GitHub Copilot provider

`copilot-provider` is an ACP provider component that talks to the **GitHub
Copilot** chat API instead of Ollama. It's a `yosh:acp/provider` guest that the
[workspace host](../../README.md) loads like any other provider.

> **Run every command below from the workspace root** (the repository
> top-level), not from this crate directory.

## Build and run

```shell
# build the copilot provider
cargo build -p copilot-provider --target wasm32-wasip2 --release

# one-time: store a GitHub token from an account with a Copilot subscription
# (a `gh auth token` from the GitHub CLI works — see "Authentication")
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token

# run the host against the Copilot API
cargo run -p host -- --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

## Authentication

The guest needs a GitHub token from an account with an **active Copilot
subscription**. A `gh auth token` from the GitHub CLI works directly — there is
no separate "grant" step and no editor OAuth-app dance.

**Accepted token types:** OAuth (`gho_…`), GitHub App (`ghu_…`), and
fine-grained PATs (`github_pat_…` with the *Copilot Requests* permission).
Classic PATs (`ghp_…`) are rejected up front.

**How the token is used.** The guest first tries GitHub's editor token-exchange
endpoint (`GET /copilot_internal/v2/token`). That endpoint only accepts tokens
minted by a Copilot-enabled *editor* OAuth app, so for a GitHub CLI token or a
fine-grained PAT it returns `404`. The guest treats that as "exchange
unavailable" and falls back to sending the GitHub token **directly** to the
chat API (`https://api.githubcopilot.com`) as a bearer token, which those
tokens are accepted for. So both kinds of token just work — you never have to
care which path ran.

Provision the token into the provider's secret store (identity
`local:copilot_provider` when loaded from a file — see
[Secrets](../../README.md#secrets)); the value is read from stdin:

```shell
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token
```

Verify it landed — exit 0 = set, 1 = unset; the token is never printed:

```shell
cargo run -p host -- secret check local:copilot_provider github_token
```

Then run the host normally; no secrets flag is needed:

```shell
cargo run -p host -- \
    --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

If no secret is stored, the guest falls back to the `COPILOT_GITHUB_TOKEN`,
`GH_TOKEN`, or `GITHUB_TOKEN` environment variables (in that order) — handy for
CI (`GH_TOKEN=$(gh auth token)`).

## Tools

Unlike the Ollama provider (a pure text relay), the Copilot provider runs an
**agentic loop**: on every prompt it advertises two file-editing tools to the
model and lets it call them for up to eight rounds before answering.

| Tool              | ACP method            | Kind   |
|-------------------|-----------------------|--------|
| `read_text_file`  | `fs/read_text_file`   | `read` |
| `write_text_file` | `fs/write_text_file`  | `edit` |

Both are always advertised; there is intentionally **no terminal/command tool**.
Relative paths are resolved against the session `cwd`.

Each call is surfaced to the editor as a tool-call card — an initial
`tool_call` update (status *pending*), then a `tool_call_update` when it
completes or fails. Writes carry a `diff` so the editor can render the change.

Before the guest touches the filesystem it asks the editor for permission via
`session/request_permission`, offering four choices: *allow once*, *allow always*,
*reject once*, *reject always*. The two *always* choices are remembered for the
rest of the session (per tool), so you're only prompted once per tool. If the
editor doesn't support a tool or denies permission, the model is told and can
continue without it.

## Mode, model, thinking, usage, cost, and approval

Every session exposes config-option selectors (shown by editors that render
them), and every prompt turn reports context-window usage — all **sourced from
upstream Copilot data or backed by real provider behavior, never fabricated**:

- **Mode** — a `mode` selector (categorised as a *mode*) mirroring the GitHub
  Copilot CLI's modes: **Agent** (default conversational), **Plan** (steers the
  model toward proposing a step-by-step plan and injects a per-turn directive to
  avoid making changes), and **Autopilot** (autonomous; implies *Allow All* so
  tool calls run without prompting). Defaults to **Agent** on every new session.
- **Model** — a `model` selector listing the chat models your account can use
  (`GET /models`, de-duplicated). A new session defaults to the **last model
  and thinking level you selected** (persisted to `/data/preferences.json`, and
  seeded from your most recent saved session on first run); it falls back to
  `COPILOT_MODEL` only when no prior choice exists.
- **Thinking** — a `reasoning-effort` selector (categorised as a *thought
  level*) offering the levels the selected model advertises under
  `capabilities.supports.reasoning_effort` (e.g. *low* / *medium* / *high*).
  Models without native reasoning control (e.g. `gpt-4o`) show no levels, and
  the effort is only sent to models that accept it. Because the last-used model
  is remembered, this selector is present **from the start of a new session**
  whenever that model supports reasoning — not only after switching models.
- **Context usage** — the provider emits a `usage_update` with `used` (the
  response's `total_tokens`) and `size` (the model's
  `capabilities.limits.max_context_window_tokens`), so the editor can render a
  context-% indicator (`used / size`). The meter is emitted **at the start of
  every turn** (before any tokens stream) at its last-known value — `0` for a
  brand-new session — and again at the end with the turn's real figures. This
  keeps the editor's UI stable: the meter is present the instant prompting
  begins and simply updates in place, instead of popping in once the response
  finishes. The `used`/cost values are persisted per session so a resumed
  session (and each subsequent turn) renders the meter from its last value
  rather than flashing back to `0`. Requested via `stream_options.include_usage`;
  skipped when the model advertises no window.

  > `session-update`s can only flow on a prompt turn's stream (see
  > `client.wit`), so the earliest the provider can surface the meter is the
  > start of the first prompt — there is no channel to emit it at `session/new`
  > time, before the user prompts.
- **Cost** — the `usage_update` carries a `cost` sourced from the chat
  stream's `copilot_usage.total_nano_aiu` (requested via
  `stream_options.include_usage`). GitHub deprecated premium-request
  multipliers in favor of **usage-based billing** measured in **AI Units
  (AIU)**; the provider sums each turn's `total_nano_aiu` (nano-AIU / 1e9) into
  the session's running total and reports it with `currency: "AIU"` — a real
  usage unit rather than a fabricated monetary figure, so it deliberately does
  **not** use an ISO-4217 code. It is `0` for included models and for accounts
  on unlimited/usage-based plans (which still see the `0 AIU` meter, confirming
  the signal works).
- **Allow All** — an `allow-all` toggle (categorised as *permissions*) with
  **On** / **Off**. When **On**, tool calls are approved automatically instead
  of prompting the client via `session/request_permission`; **Off** (the safe
  default on every new session) requires per-call approval. Autopilot mode
  forces this **On**. This is backed by real behavior — `request_tool_permission`
  short-circuits to *allow* — not just advertised.

Chat mode is advertised as a config option (category `mode`) rather than via the
legacy session-mode methods, matching how the provider surfaces every other
selector; the host still injects a `default` session mode for clients that only
read the legacy `modes` field.

## Configuration

All optional; read from the (inherited) host environment:

| Variable                 | Default                                | Purpose                         |
|--------------------------|----------------------------------------|---------------------------------|
| `COPILOT_MODEL`          | `gpt-4o`                                | Fallback model id (used only when no prior selection is saved) |
| `COPILOT_BASE_URL`       | from token, else `api.githubcopilot.com` | Override the API base URL     |
| `COPILOT_TOKEN_URL`      | GitHub `copilot_internal/v2/token`      | Override the token-exchange endpoint (chiefly for tests) |
| `COPILOT_EDITOR_VERSION` | `vscode/1.104.1`                       | `Editor-Version` header         |
| `COPILOT_INTEGRATION_ID` | `vscode-chat`                          | `Copilot-Integration-Id` header |

## Smoke test

```shell
cargo build -p copilot-provider --target wasm32-wasip2 --release
# A `gh auth token` from the GitHub CLI works (account needs a Copilot
# subscription); see "Authentication".
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"<id-from-session/new>","prompt":[{"type":"text","text":"hi"}]}}' \
  | cargo run -p host -- \
      --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

Use the `sessionId` returned by the `session/new` response in the
`session/prompt` call. This example prompts for plain text, but the provider is
**agentic**: ask it to read or edit a file and it will call the
`read_text_file` / `write_text_file` tools (with editor permission prompts) —
see [Tools](#tools). Image and audio content are still dropped.
