<h1 align="center">playground-wasm-acp</h1>

This is a demonstration showing how the ACP protocol can be backed by a wasm
runtime.

## Installation

In order to build this project, make sure you have a working WASI 0.2 Rust
toolchain and bindings generation

```shell
cargo install wit-bindgen                # To generate the bindings
rustup toolchain install wasm32-wasip2   # To build the `guest` crate
```

## Commands

To build the project, the following commands are available:

```shell
cargo xtask build  # Build the guest and host programs
cargo xtask check  # Check the guest and host programs
cargo xtask run    # Build the guest and host and run the guest on the host
```

## Crates

This repository contains three crates, each serving a distinct purpose:

- `crates/host` - this is the host runtime, currently implemented with Tokio.
This needs to be compiled once for each target platform.
- `crates/guest` - this is the application code which will run inside of the
host runtime. This will be calling the WASI syscall layer to communicate with
the underlying system. Crucially: it is not tied to any specific host runtime:
it can run on any host runtime we provide it with, as long as the host
implements the necessary WASI APIs.
- `crates/xtask` - the project-specific [`cargo xtask`
sub-command](https://github.com/matklad/cargo-xtask), making it possible to
build the host, guest, and runtime crate in a single command - and run programs
directly.

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
