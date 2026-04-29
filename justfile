# Generate WIT bindings for the guest crate.
bindgen: bindgen-guest

# Build and open docs for the guest bindings.
doc-guest:
    cargo doc -p acp-wasm-sys --no-deps --open

# Generate guest bindings (agent-plugin world).
bindgen-guest:
    rm -rf crates/acp-wasm-sys/wit
    cp -r vendor/wit crates/acp-wasm-sys/wit
    wit-bindgen rust crates/acp-wasm-sys/wit \
        --world agent-plugin \
        --runtime-path wit_bindgen_rt \
        --pub-export-macro \
        --out-dir crates/acp-wasm-sys/src \
        --format
    mv crates/acp-wasm-sys/src/agent_plugin.rs crates/acp-wasm-sys/src/bindings.rs
    rm -rf crates/acp-wasm-sys/wit
