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
regenerating bindings (only required if `vendor/wit/*.wit` changes).

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
just build          # build guest wasm component + host binary
just build-guest    # build only the guest (release, wasm32-wasip2)
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

## Smoke test

With Ollama running, drive the host with a fixture on stdin:

```shell
just build
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{"fs":{"readTextFile":false,"writeTextFile":false},"terminal":false}}}' \
  '{"jsonrpc":"2.0","id":2,"method":"session/new","params":{"cwd":"/tmp","mcpServers":[]}}' \
  '{"jsonrpc":"2.0","id":3,"method":"session/prompt","params":{"sessionId":"ollama-session-0","prompt":[{"type":"text","text":"hi"}]}}' \
  | cargo run -p host -- target/wasm32-wasip2/release/guest.wasm
```

Expect an `initialize` response, a `session/new` response, a sequence of
`session/update` notifications streaming the assistant's reply, and finally
the `session/prompt` response with `stopReason: "end_turn"`.

## Crates

- [`crates/acp-wasm-sys`](crates/acp-wasm-sys) — auto-generated WIT bindings
  for the `agent-plugin` world (regenerate with `just bindgen-guest`).
- [`crates/guest`](crates/guest) — the wasm component: implements the ACP
  `agent` interface, calls Ollama via `wstd::http`, keeps per-session
  conversation history.
- [`crates/host`](crates/host) — the wasmtime host: instantiates the guest,
  speaks ACP JSON-RPC over stdio, translates between WIT and ACP schema types.

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

