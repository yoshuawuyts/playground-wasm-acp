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

# one-time: store a Copilot-entitled GitHub token (see "Authentication" —
# a plain `gh auth token` is not entitled and will 404 at exchange)
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token

# run the host against the Copilot API
cargo run -p host -- --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

## Authentication

The guest needs a raw GitHub token, which it exchanges at runtime for a
short-lived Copilot API token (`GET /copilot_internal/v2/token`). **Two
conditions must both hold**, or the exchange fails:

- **Token type** — OAuth (`gho_…`), GitHub App (`ghu_…`), or a fine-grained
  PAT (`github_pat_…` with the *Copilot Requests* permission). Classic PATs
  (`ghp_…`) are rejected up front.
- **Copilot entitlement** — the token's OAuth app must be authorized for
  Copilot *and* the account must have an active Copilot subscription. A token
  that authenticates but isn't entitled makes the exchange return **`404 Not
  Found`** (not `403`). In particular a **`gh auth token` from the GitHub CLI
  is _not_ Copilot-entitled** — it's a valid `gho_…` token, so it passes the
  prefix check, but it 404s at exchange.

The reliable way to mint an *entitled* token is GitHub's device flow against a
Copilot editor OAuth app (`client_id=Iv1.b507a08c87ecfe98`, the client editor
integrations use). This runs the flow and stores the result in the provider's
secret store (identity `local:copilot_provider` when loaded from a file — see
[Secrets](../../README.md#secrets)); it needs `curl` and `python3`:

```shell
cid=Iv1.b507a08c87ecfe98
resp=$(curl -s https://github.com/login/device/code \
  -d "client_id=$cid&scope=read:user" -H 'accept: application/json')
device_code=$(printf %s "$resp" | python3 -c 'import json,sys; print(json.load(sys.stdin)["device_code"])')
printf %s "$resp" | python3 -c 'import json,sys; d=json.load(sys.stdin); print("Open", d["verification_uri"], "and enter code:", d["user_code"])'

# Authorize in the browser, then poll until the token is issued:
until token=$(curl -s https://github.com/login/oauth/access_token \
      -d "client_id=$cid&device_code=$device_code&grant_type=urn:ietf:params:oauth:grant-type:device_code" \
      -H 'accept: application/json' \
      | python3 -c 'import json,sys; print(json.load(sys.stdin).get("access_token",""))') \
    && [ -n "$token" ]; do sleep 5; done

printf %s "$token" | cargo run -p host -- secret set local:copilot_provider github_token
unset token
```

Verify it landed — exit 0 = set, 1 = unset; the token is never printed:

```shell
cargo run -p host -- secret check local:copilot_provider github_token
```

If your GitHub CLI login *is* Copilot-entitled, the shorter
`gh auth token | cargo run -p host -- secret set local:copilot_provider github_token`
works too — but if it 404s at exchange, fall back to the device flow above.

Then run the host normally; no secrets flag is needed:

```shell
cargo run -p host -- \
    --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

If no secret is stored, the guest falls back to the `COPILOT_GITHUB_TOKEN`,
`GH_TOKEN`, or `GITHUB_TOKEN` environment variables (in that order).

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
# Provision a Copilot-entitled token first (see "Authentication"); a plain
# `gh auth token` will 404 at exchange if it isn't entitled.
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
