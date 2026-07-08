# Generate all WIT bindings for the acp-wasm-sys crate (provider + layer worlds).
bindgen: bindgen-provider bindgen-layer

# Build everything: provider + layer wasm components + host binary.
build: build-providers build-layers build-host

# Build the provider wasm components (release).
build-providers:
    cargo build -p ollama-provider --target wasm32-wasip2 --release
    cargo build -p copilot-provider --target wasm32-wasip2 --release

# Build the layer wasm components (release).
build-layers:
    cargo build -p uppercase-layer --target wasm32-wasip2 --release
    cargo build -p plan-layer --target wasm32-wasip2 --release

# Build the host binary.
build-host:
    cargo build -p host

# Run the end-to-end smoke tests. Builds wasm components + host first.
# Tests run serial-by-default to keep host stderr ordering legible; pass
# `-- --test-threads=N` after the recipe to override.
test-e2e: build
    cargo test -p e2e-tests -- --test-threads=1 --nocapture

# Build everything, then run the host with the uppercase layer wrapping the
# ollama provider. Extra args are forwarded to stdin (use `just run < fixture.jsonl`).
run: build
    cargo run -p host -- \
        --provider target/wasm32-wasip2/release/ollama_provider.wasm \
        --layer target/wasm32-wasip2/release/uppercase_layer.wasm

# Build and open docs for the acp-wasm-sys bindings.
doc-provider:
    cargo doc -p acp-wasm-sys --no-deps --open

# Verify the installed `wit-bindgen` CLI matches the workspace `wit-bindgen`
# crate version. A mismatched CLI silently emits bindings that don't compile
# against the pinned runtime (e.g. a 0.41 CLI references `AsyncWaitResult`,
# which was removed by 0.54), so fail fast with an actionable message instead.
_check-wit-bindgen:
    #!/usr/bin/env bash
    set -euo pipefail
    want="$(grep -m1 '^wit-bindgen = ' Cargo.toml | sed -E 's/.*"([0-9.]+)".*/\1/')"
    have="$(wit-bindgen --version | sed -E 's/.*[[:space:]]([0-9.]+)$/\1/')"
    if [ "$want" != "$have" ]; then
        echo "error: wit-bindgen CLI $have does not match workspace crate $want" >&2
        echo "install the matching CLI: cargo install wit-bindgen-cli --version $want --locked" >&2
        exit 1
    fi

# Generate the provider-world bindings (shared by the ollama + copilot providers).
bindgen-provider: _check-wit-bindgen
    wit-bindgen rust wit/acp \
        --world provider \
        --runtime-path wit_bindgen::rt \
        --pub-export-macro \
        --generate-all \
        --out-dir crates/acp-wasm-sys/src \
        --format

# Generate the layer-world bindings (shared by the uppercase + plan layers).
#
# After generation we rename the layer's `agent` cabi export macro to
# avoid a `#[macro_export]` collision with the provider world's macro
# of the same name (both worlds export `agent`, both files end up at
# the same crate root). The `client` cabi macro is unique to the
# layer and needs no rename.
bindgen-layer: _check-wit-bindgen
    wit-bindgen rust wit/acp \
        --world layer \
        --runtime-path wit_bindgen::rt \
        --pub-export-macro \
        --generate-all \
        --out-dir crates/acp-wasm-sys/src \
        --format
    sed -i '' 's|__export_yosh_acp_agent_7_0_0_cabi|__export_yosh_acp_agent_7_0_0_cabi_layer|g' crates/acp-wasm-sys/src/layer.rs
