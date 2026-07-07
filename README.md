<h1 align="center">playground-wasm-acp</h1>

A demonstration showing how the [Agent Client Protocol] (ACP) can be backed by
a wasm component running inside a [wasmtime] host. The host bridges the ACP
JSON-RPC wire protocol on stdio to/from the wasm guest, which forwards prompts
to a local Ollama server for LLM inference.

[Agent Client Protocol]: https://agentclientprotocol.com/
[wasmtime]: https://wasmtime.dev/

## Architecture

```
   editor                host (wasmtime)              guest (wasm)
+----------+   stdio   +-----------------+   WIT    +----------------+
| ACP      |  JSON-RPC | agent-client-   | bindgen  | acp-wasm-sys   |
| client   |<--------->| protocol        |<-------->| (WIT bindings) |
| (e.g.    |  v1       | Builder + ACP   |          |                |
|  Zed)    |           | Agent role      |          | Ollama client  |
+----------+           +-----------------+          | (wstd::http)   |
                              |                     +----------------+
                              | wasi:http                    |
                              v                              v
                         outbound HTTP -----> http://localhost:11434
```

The wasm guest implements the `agent-plugin` WIT world, exporting the ACP
`agent` interface and importing the `client` interface. The host generates
inverse bindings (it implements `client`, calls into `agent`) and translates
between the WIT types and `agent_client_protocol::schema` types in
[`crates/host/src/translate.rs`](crates/host/src/translate.rs).

## Installation

You need a Rust toolchain plus the `wasm32-wasip2` target and `wit-bindgen` for
regenerating bindings (only required if `wit/*.wit` changes).

```shell
rustup target add wasm32-wasip2
cargo install wit-bindgen-cli   # only needed to regenerate bindings
cargo install just              # task runner
```

You'll also need a running [Ollama](https://ollama.com) instance with at least
one model pulled:

```shell
ollama serve &
ollama pull llama3.2
```

## Commands

The repo uses [`just`](https://github.com/casey/just) for common tasks:

```shell
just build          # build the ollama-provider wasm component + host binary
just build-guest    # build only the ollama-provider (release, wasm32-wasip2)
just build-host     # build only the host
just run            # build everything, then run the host on stdio
just bindgen-guest  # regenerate WIT bindings into crates/acp-wasm-sys
just doc-guest      # cargo doc for the WIT bindings crate
```

Run the test suite with `cargo test -p host` (covers the WIT ↔ ACP type
translation in [`crates/host/src/translate.rs`](crates/host/src/translate.rs)).

## Configuration

The guest reads two environment variables at runtime; both are optional:

| Variable       | Default                              | Purpose                |
|----------------|--------------------------------------|------------------------|
| `OLLAMA_URL`   | `http://localhost:11434/api/chat`    | Ollama `/api/chat` URL |
| `OLLAMA_MODEL` | `llama3.2`                           | Model to use           |

Host log verbosity is controlled by `RUST_LOG` (e.g. `RUST_LOG=host=debug`),
defaulting to `host=info`.

### Secrets

The host implements the `wasmcloud:secrets/store` + `reveal` WIT interfaces
for the guest. Every component that imports `wasmcloud:secrets` transparently
gets its **own private secret store, indexed by its component identity**. A
`store.get(key)` resolves only against *that component's* store, so one
component can never read another's secrets — the host supplies the identity,
the guest never names it. There is no config file.

A component's identity is **`namespace:component-name`**:

- a registry component uses its WIT `namespace:package`, e.g.
  `yosh:ollama-provider` (the version is stripped, so secrets survive upgrades);
- a component loaded from a file has no registry namespace, so it becomes
  `local:<filename-stem>`, e.g. `local:ollama_provider`.

The namespace means two components that share a bare name but come from
different sources get separate stores. The host logs the resolved identity for
each stage at startup (`provider=…`, `layer=…`).

Secrets live in a [`keyring-core`] credential store — an OS keychain by
default. Each identity maps to keyring
`service = "<prefix>:<namespace>:<component-name>"` (the per-component store)
with `user = <key>` for each entry. The `prefix` (default `wasm-acp`, override
with `--keyring-service-prefix`) keeps this host's entries from colliding with
other apps in a shared keychain. Stored bytes are returned to the guest as a
`string` when they are valid UTF-8, otherwise as raw `bytes`. Resolved values
never appear in logs and are cached for the host process lifetime.

Select the backing store with `--keyring-store <native|mock>`:

| Backend  | Store                                                             |
|----------|------------------------------------------------------------------|
| `native` | macOS Keychain / Windows Credential Manager / Linux Secret Service (default) |
| `mock`   | in-memory, non-persistent (tests/CI; a fresh process starts empty) |

The store is initialized once at startup; the keychain itself is not touched
until a secret is read or written (which may surface an OS prompt).

#### Provisioning

The WIT interface is read-only, so populate a component's store with the
`secret` admin subcommands. They take the full `namespace:component-name`
identity and use the same `--keyring-store` / `--keyring-service-prefix` as the
run path:

```shell
# Store api_key for a file-loaded provider (value read from stdin).
printf 'sk-...' | cargo run -p host -- secret set local:ollama_provider api_key

# A registry component is addressed by its WIT namespace:package.
printf 'sk-...' | cargo run -p host -- secret set yosh:ollama-provider api_key

# Store a raw-bytes secret verbatim (no trailing-newline stripping).
head -c 32 /dev/urandom | cargo run -p host -- secret set local:ollama_provider seed --bytes

# Remove a secret (idempotent).
cargo run -p host -- secret delete local:ollama_provider api_key

# Check whether a secret is set, without revealing it
# (exits 0 if set, 1 if not — usable as a shell predicate).
cargo run -p host -- secret check local:ollama_provider api_key
```

A single trailing newline is stripped from string values, so `printf 'x\n'`
and `printf 'x'` store the same secret; pass `--bytes` to store stdin exactly.
The component then reads it back via `store.get("api_key")`. `secret check`
probes existence without decrypting the value, so it never reads (or prompts
for) the secret it reports on.

[`keyring-core`]: https://docs.rs/keyring-core

## Smoke test

With Ollama running, drive the host with a fixture on stdin:

```shell
just build
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"ollama-session-0","prompt":[{"type":"text","text":"hi"}]}}' \
  | cargo run -p host -- target/wasm32-wasip2/release/ollama_provider.wasm
```

Expect an `initialize` response, a `session/new` response, a sequence of
`session/update` notifications streaming the assistant's reply, and finally
the `session/prompt` response with `stopReason: "end_turn"`.

## GitHub Copilot provider

[`crates/copilot-provider`](crates/copilot-provider) is an alternative provider
that speaks to the **GitHub Copilot** chat API instead of Ollama. Build and run
it with:

```shell
# build the copilot provider
cargo build -p copilot-provider --target wasm32-wasip2 --release

# one-time: store your GitHub token in the provider's secret store
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token

# run the host against the Copilot API
cargo run -p host -- --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

### Authentication

The guest needs a raw GitHub token, which it exchanges at runtime for a
short-lived Copilot API token (`GET /copilot_internal/v2/token`). Accepted
token types: OAuth (`gho_…`), GitHub App (`ghu_…`), and fine-grained PATs
(`github_pat_…` with the *Copilot Requests* permission). Classic PATs
(`ghp_…`) are **not** accepted by the Copilot API.

Provide the token via the host secret store (preferred). The guest reads it
back as `github_token` from its own per-component store — see [Secrets](#secrets)
for how identity and provisioning work. Loaded from a file, the provider's
identity is `local:copilot_provider`, so populate its store with the `secret`
admin subcommand (token read from stdin):

```shell
# Simplest source: the gh CLI.
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token

# Verify it's stored (exit 0 = set, 1 = unset); the token is never printed.
cargo run -p host -- secret check local:copilot_provider github_token
```

Then run the host normally; no secrets flag is needed:

```shell
cargo run -p host -- \
    --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

If no secret is stored, the guest falls back to the `COPILOT_GITHUB_TOKEN`,
`GH_TOKEN`, or `GITHUB_TOKEN` environment variables (in that order).

### Configuration

All optional; read from the (inherited) host environment:

| Variable                 | Default                                | Purpose                         |
|--------------------------|----------------------------------------|---------------------------------|
| `COPILOT_MODEL`          | `gpt-4o`                                | Default model id                |
| `COPILOT_BASE_URL`       | from token, else `api.githubcopilot.com` | Override the API base URL     |
| `COPILOT_EDITOR_VERSION` | `vscode/1.104.1`                       | `Editor-Version` header         |
| `COPILOT_INTEGRATION_ID` | `vscode-chat`                          | `Copilot-Integration-Id` header |

### Smoke test

```shell
cargo build -p copilot-provider --target wasm32-wasip2 --release
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

## Crates

- [`crates/acp-wasm-sys`](crates/acp-wasm-sys) — auto-generated WIT bindings
  for the `agent-plugin` world (regenerate with `just bindgen-guest`).
- [`crates/ollama-provider`](crates/ollama-provider) — the wasm component:
  implements the ACP `agent` interface, calls Ollama via `wstd::http`, keeps
  per-session conversation history.
- [`crates/copilot-provider`](crates/copilot-provider) — an alternative wasm
  component that talks to the **GitHub Copilot** chat API: resolves a GitHub
  token (host secrets store or env), exchanges it for a short-lived Copilot
  API token, and streams OpenAI-compatible chat completions. See
  [GitHub Copilot provider](#github-copilot-provider).
- [`crates/host`](crates/host) — the wasmtime host: instantiates the
  ollama-provider component, speaks ACP JSON-RPC over stdio, translates
  between WIT and ACP schema types.

## Limitations

The MVP intentionally cuts a few corners:

- **Text only.** Image, audio, resource-link, and embedded-resource content
  blocks are dropped both directions.
- **Cancellation is host-side only.** A `session/cancel` notification drops
  the host's `await` on the wasm prompt and returns `stopReason: cancelled`,
  but the wasm guest itself doesn't get an interrupt — any in-flight HTTP
  request to Ollama still completes (its result is just discarded).
- **No terminal/permission methods.** The host returns `method-not-found`
  for `terminal/*` and `session/request_permission`. `fs/read_text_file`
  and `fs/write_text_file` are wired through to the editor.
- **No MCP servers.** Servers passed in `session/new` are accepted but the
  guest doesn't connect to them.

## License

<sup>
Licensed under <a href="LICENSE">Apache-2.0 WITH LLVM-exception</a>
</sup>
<br/>
<sub>
Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license with LLVM-exception,
shall be licensed as above, without any additional terms or conditions.
</sub>

