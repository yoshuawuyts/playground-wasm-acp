# Generate WIT bindings for the guest and host crates.
bindgen: bindgen-guest bindgen-host

# Generate guest bindings (agent-plugin world).
bindgen-guest:
    rm -rf crates/guest/wit
    cp -r vendor/wit crates/guest/wit
    wit-bindgen rust crates/guest/wit \
        --world agent-plugin \
        --out-dir crates/guest/src \
        --format
    mv crates/guest/src/agent_plugin.rs crates/guest/src/bindings.rs
    rm -rf crates/guest/wit

# Generate host bindings (client-host world).
bindgen-host:
    rm -rf crates/host/wit
    cp -r vendor/wit crates/host/wit
    wit-bindgen rust crates/host/wit \
        --world client-host \
        --out-dir crates/host/src \
        --format
    mv crates/host/src/client_host.rs crates/host/src/bindings.rs
    rm -rf crates/host/wit
