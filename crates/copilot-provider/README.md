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

## Configuration

All optional; read from the (inherited) host environment:

| Variable                 | Default                                | Purpose                         |
|--------------------------|----------------------------------------|---------------------------------|
| `COPILOT_MODEL`          | `gpt-4o`                                | Default model id                |
| `COPILOT_BASE_URL`       | from token, else `api.githubcopilot.com` | Override the API base URL     |
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
`session/prompt` call. Like the Ollama provider, this MVP is **text only** — it
streams assistant text and does not yet surface tool calls, images, or audio.
