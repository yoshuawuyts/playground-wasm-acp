use std::path::PathBuf;

use clap::Parser;
use wasmtime::{Result, component::Component, *};
use wasmtime_wasi::{WasiCtxView, WasiView};

wasmtime::component::bindgen!("local:demo/hello");

#[derive(Parser)]
struct Args {
    /// The path to our `.wasm` component
    wasm_path: PathBuf,
    program_input: String,
}

/// The shared context for our component instantiation.
///
/// Each store owns one of these structs. In the linker this maps: names in the
/// component -> functions on the host side.
struct Ctx {
    // Anything that WASI can access is mediated though this. This contains
    // capabilities, preopens, etc.
    wasi: wasmtime_wasi::WasiCtx,
    // NOTE: this might go away eventually
    // We need something which owns the host representation of the resources; we
    // store them in here. Think of it as a `HashMap<i32, Box<dyn Any>>`
    table: wasmtime::component::ResourceTable,
}
impl WasiView for Ctx {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi,
            table: &mut self.table,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Get the CLI args
    let args = Args::parse();

    // Setup the engine.
    // These pieces can be reused for multiple component instantiations.
    let mut config = Config::default();
    config.wasm_component_model(true);
    config.async_support(true);
    let engine = Engine::new(&config)?;
    let component = Component::from_file(&engine, args.wasm_path)?;

    // Setup the linker and add the `wasi:cli/command` world's imports to this
    // linker.
    let mut linker: component::Linker<Ctx> = component::Linker::new(&engine);
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Instantiate the component!
    let host = Ctx {
        wasi: wasmtime_wasi::WasiCtxBuilder::new()
            .inherit_stderr()
            .inherit_stdout()
            .inherit_network()
            .build(),
        table: wasmtime::component::ResourceTable::new(),
    };
    let mut store: Store<Ctx> = Store::new(&engine, host);

    // Instantiate the component and we're off to the races.

    let hello = Hello::instantiate(&mut store, &component, &linker)?;

    // Run our component!
    let result = hello
        .local_demo_main()
        .call_run(&mut store, &args.program_input)?;
    println!("{result:?}");
    Ok(())
}
