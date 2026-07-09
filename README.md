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

### Terminal execution

Whether the guest may run terminal (CLI) commands is controlled by a
**host-owned boolean session config option** (`id: "terminal"`, `type:
"boolean"`) that defaults to `false`. It is advertised in the
`configOptions` of every `session/new` and `session/load` response — but
only to clients that opted into boolean config options by advertising
`session.configOptions.boolean` in their `initialize` capabilities (per the
[boolean-config-option RFD](https://agentclientprotocol.com/rfds/boolean-config-option),
some clients break on unknown value shapes). Clients that don't opt in
never see the option and terminal execution stays off.

The toggle is host-owned end to end: no guest provider advertises it, and
the host intercepts its setter before it reaches any guest — a component
can't grant itself host CLI access. Enable it with a boolean
`session/set_config_option`:

```console
# initialize opting into boolean config options
'{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"session":{"configOptions":{"boolean":{}}}}}}'
# …after session/new returns a sessionId, turn terminal tools on:
'{"jsonrpc":"2.0","id":2,"method":"session/set_config_option","params":{"sessionId":"<id>","configId":"terminal","type":"boolean","value":true}}'
```

While the option is `false` the host refuses to spawn any process and
surfaces that to the guest. Once toggled on, each `client.terminal` call
spawns `command args...` with the guest-supplied environment and working
directory, streams the combined stdout/stderr back to the guest as it
arrives, and reports the exit code (or terminating signal name on Unix).
Dropping the terminal resource kills the process. The optional
`output-byte-limit` is honored as an upper bound (excess output is
dropped). Because the command runs on the host with the host's privileges,
only enable this for components you trust to run arbitrary local commands.

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
that speaks to the **GitHub Copilot** chat API instead of Ollama:

```shell
# build the copilot provider
cargo build -p copilot-provider --target wasm32-wasip2 --release

# one-time: store a GitHub token from an account with a Copilot subscription —
# a `gh auth token` from the GitHub CLI works (see the provider README)
gh auth token | cargo run -p host -- secret set local:copilot_provider github_token

# run the host against the Copilot API
cargo run -p host -- --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

Unlike the Ollama provider, the Copilot provider is **agentic**: it advertises
`read_text_file` and `write_text_file` tools to the model, surfaces each call to
the editor as a tool-call card, and asks the editor for permission before
touching the filesystem (with per-session *allow always* / *reject always*
memory).

It also surfaces per-turn **context-window usage** and **usage-based cost** (in
GitHub AI Units, AIU) as ACP `usage_update`s, plus **mode** (agent / plan /
autopilot), **model**, **thinking-effort**, and **allow-all** (auto tool
approval) config selectors — mirroring the selectors the GitHub Copilot CLI
exposes over ACP, and each backed by upstream data or real provider behavior.

See **[`crates/copilot-provider/README.md`](crates/copilot-provider/README.md)**
for GitHub token authentication (token types and how the guest falls back from
the editor token-exchange to direct-token auth), the file-editing tools,
configuration, and a full smoke test.

## Loading multiple providers

Pass `--provider` more than once to load several providers into one host. Each
session then instantiates one wasm chain **per provider** (the same `--layer`
stack wraps every provider, and each provider keeps its own private secret store
and project `/data`), and the host **merges every provider's model selector into
a single cross-provider `Model` dropdown** — each entry labelled by the provider
that owns it:

```shell
cargo run -p host -- \
  --provider target/wasm32-wasip2/release/ollama_provider.wasm \
  --provider target/wasm32-wasip2/release/copilot_provider.wasm
```

The user then picks *which model from which provider* backs the session, e.g.
`llama3.2 (local:ollama_provider)` or `gpt-4o (local:copilot_provider)`. The
provider owning the **selected** model is the *active* provider: it backs
prompts, mode switches, and the non-model selectors (thinking-effort,
allow-all, …). Selecting a model from a different provider switches the active
provider, and its own selectors take over.

With a single `--provider` the host is a transparent passthrough: config
options, values, and ids are forwarded verbatim, so single-provider behaviour is
unchanged.

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
- [`crates/host`](crates/host) — the wasmtime host: instantiates one or more
  provider components (merging their model selectors when several are loaded),
  speaks ACP JSON-RPC over stdio, translates between WIT and ACP schema types.

## Limitations

The MVP intentionally cuts a few corners:

- **Text only.** Image, audio, resource-link, and embedded-resource content
  blocks are dropped both directions.
- **Cancellation is host-side only.** A `session/cancel` notification drops
  the host's `await` on the wasm prompt and returns `stopReason: cancelled`,
  but the wasm guest itself doesn't get an interrupt — any in-flight HTTP
  request to Ollama still completes (its result is just discarded).
- **Config-gated terminal execution.** The host services the guest's
  `client.terminal` resource by running the requested command as a *local*
  child process — combined stdout/stderr is streamed back to the guest and the
  exit code (or terminating signal) is reported — rather than forwarding ACP
  `terminal/*` methods to the editor. It is gated by a host-owned `terminal`
  boolean session config option (default `false`), advertised only to clients
  that opt into boolean config options: until it is toggled on the host refuses
  to spawn any process, so an untrusted component can't run local commands. See
  [Terminal execution](#terminal-execution).
- **Editor-backed filesystem & permissions.** `fs/read_text_file`,
  `fs/write_text_file`, and `session/request_permission` are wired through to
  the editor — the copilot-provider uses all three to offer permissioned file
  editing.
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

