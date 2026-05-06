# Generate WIT bindings for the ollama-provider crate.
bindgen: bindgen-provider bindgen-layer

# Build everything: ollama-provider + uppercase-layer wasm components + host binary.
build: build-provider build-layer build-host

# Build the ollama-provider wasm component (release).
build-provider:
    cargo build -p ollama-provider --target wasm32-wasip2 --release

# Build the uppercase-layer wasm component (release).
build-layer:
    cargo build -p uppercase-layer --target wasm32-wasip2 --release

# Build the host binary.
build-host:
    cargo build -p host

# Build everything, then run the host with the uppercase layer wrapping the
# ollama provider. Extra args are forwarded to stdin (use `just run < fixture.jsonl`).
run: build
    cargo run -p host -- \
        --provider target/wasm32-wasip2/release/ollama_provider.wasm \
        --layer target/wasm32-wasip2/release/uppercase_layer.wasm

# Build and open docs for the ollama-provider bindings.
doc-provider:
    cargo doc -p acp-wasm-sys --no-deps --open

# Generate ollama-provider bindings (provider world).
bindgen-provider:
    rm -rf crates/acp-wasm-sys/wit
    cp -r vendor/wit crates/acp-wasm-sys/wit
    wit-bindgen rust crates/acp-wasm-sys/wit \
        --world provider \
        --runtime-path wit_bindgen_rt \
        --pub-export-macro \
        --out-dir crates/acp-wasm-sys/src \
        --format
    rm -rf crates/acp-wasm-sys/wit

# Generate uppercase-layer bindings (layer world).
#
# After generation we rename the layer's `agent` cabi export macro to
# avoid a `#[macro_export]` collision with the provider world's macro
# of the same name (both worlds export `agent`, both files end up at
# the same crate root). The `client` cabi macro is unique to the
# layer and needs no rename.
bindgen-layer:
    rm -rf crates/acp-wasm-sys/wit
    cp -r vendor/wit crates/acp-wasm-sys/wit
    wit-bindgen rust crates/acp-wasm-sys/wit \
        --world layer \
        --runtime-path wit_bindgen_rt \
        --pub-export-macro \
        --out-dir crates/acp-wasm-sys/src \
        --format
    rm -rf crates/acp-wasm-sys/wit
    sed -i '' 's|__export_yoshuawuyts_acp_agent_4_0_0_cabi|__export_yoshuawuyts_acp_agent_4_0_0_cabi_layer|g' crates/acp-wasm-sys/src/layer.rs
