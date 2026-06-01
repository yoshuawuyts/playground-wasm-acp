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

### Filesystem mounts

Every project session gets a private, host-backed scratch directory preopened
at `/data`. You can expose **additional** writable directories to the agent
chain — for example a synced cloud folder used as persistent storage outside
the session's working directory — by declaring **mounts** in a global host
config file.

The config lives at the XDG config path (the same layout `install` uses for its
component cache): `$XDG_CONFIG_HOME/acp-wasm/config.toml`, falling back to the
platform config dir (`~/.config/acp-wasm/config.toml` on Linux,
`~/Library/Application Support/acp-wasm/config.toml` on macOS). If the file is
absent the host runs exactly as before, with only `/data`.

Each `[mounts.<name>]` table preopens a directory at `/<name>` for the chain:

```toml
# Preopen the host directory /var/log/myapp at /logs.
[mounts.logs]
path = "/var/log/myapp"

# Preopen your synced cloud folder at /onedrive for persistent storage.
[mounts.onedrive]
path = "/home/me/OneDrive/agent-data"
```

Rules:

- `<name>` must be a single path segment (no `/`), and cannot be the reserved
  name `data`.
- Exactly one backing key per entry. Host-directory mounts use `path` (an
  absolute path to an existing directory).
- A `component` key (pointing at a `wasi:filesystem`-exporting wasm component,
  by path or WIT name) is reserved for plugin-backed mounts — see
  *Limitations* below.

Mounts are read/write (`DirPerms::all` / `FilePerms::all`), just like `/data`.
The bundled `ollama-provider` honours an optional `ACP_DATA_ROOT` environment
variable to redirect its session-history storage from `/data` onto a mount
(e.g. `ACP_DATA_ROOT=/onedrive`), demonstrating persistence outside the cwd.

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

## Crates

- [`crates/acp-wasm-sys`](crates/acp-wasm-sys) — auto-generated WIT bindings
  for the `agent-plugin` world (regenerate with `just bindgen-guest`).
- [`crates/ollama-provider`](crates/ollama-provider) — the wasm component:
  implements the ACP `agent` interface, calls Ollama via `wstd::http`, keeps
  per-session conversation history.
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
- **Component-backed filesystem mounts are not wired yet.** Host-directory
  mounts (`path =`) work today (see *Filesystem mounts*). Mounting a
  `wasi:filesystem`-**exporting** wasm component (`component =`) is validated at
  startup but rejected with a clear error, because `wasmtime-wasi` 44 hardcodes
  filesystem preopens to real host directories: there is no trait-based virtual
  filesystem hook, and an agent reaches the filesystem through a single
  `wasi:filesystem` + `wasi:io` import whose stream resources are shared with
  stdio/http. Serving a plugin-backed mount alongside `/data` therefore requires
  a host-owned **dispatching** implementation of `wasi:filesystem` (and the
  `wasi:io` streams it returns) that routes each descriptor to either a host
  directory or the plugin's exports — a sizeable change tracked as follow-up
  work. Until then, use a `path` mount.

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

