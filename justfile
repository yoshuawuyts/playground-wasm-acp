# Generate WIT bindings for the guest and host crates.
bindgen: bindgen-guest bindgen-host

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
        --out-dir crates/acp-wasm-sys/src \
        --format
    mv crates/acp-wasm-sys/src/agent_plugin.rs crates/acp-wasm-sys/src/bindings.rs
    rm -rf crates/acp-wasm-sys/wit

# Generate host bindings (client-host world).
bindgen-host:
    rm -rf crates/host/wit
    cp -r vendor/wit crates/host/wit
    wit-bindgen rust crates/host/wit \
        --world client-host \
        --runtime-path wit_bindgen_rt \
        --out-dir crates/host/src \
        --format
    mv crates/host/src/client_host.rs crates/host/src/bindings.rs
    rm -rf crates/host/wit
