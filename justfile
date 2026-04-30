# Generate WIT bindings for the guest crate.
bindgen: bindgen-guest

# Build everything: guest wasm component + host binary.
build: build-guest build-host

# Build the guest wasm component (release).
build-guest:
    cargo build -p guest --target wasm32-wasip2 --release

# Build the host binary.
build-host:
    cargo build -p host

# Build the guest, then run the host pointed at it. Extra args are forwarded
# to the host binary's stdin (use `just run < fixture.jsonl` to drive it).
run: build
    cargo run -p host -- target/wasm32-wasip2/release/guest.wasm

# Build and open docs for the guest bindings.
doc-guest:
    cargo doc -p acp-wasm-sys --no-deps --open

# Generate guest bindings (provider world).
bindgen-guest:
    rm -rf crates/acp-wasm-sys/wit
    cp -r vendor/wit crates/acp-wasm-sys/wit
    wit-bindgen rust crates/acp-wasm-sys/wit \
        --world provider \
        --runtime-path wit_bindgen_rt \
        --pub-export-macro \
        --out-dir crates/acp-wasm-sys/src \
        --format
    mv crates/acp-wasm-sys/src/provider.rs crates/acp-wasm-sys/src/bindings.rs
    rm -rf crates/acp-wasm-sys/wit
