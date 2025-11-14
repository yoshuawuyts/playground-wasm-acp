use clap::Parser;
use xshell::{cmd, Shell};

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;

/// cargo xtask
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
enum Command {
    /// Check the guest and host
    Check,
    /// Build the guest and host
    Build(Build),
    /// Build the guest and host, then run the guest on the host
    Run(Run),
}

#[derive(Parser, Debug)]
struct Build {
    /// Whether to compile in release mode
    #[arg(long)]
    release: bool,
}

#[derive(Parser, Debug)]
struct Run {
    /// Whether to compile in release mode
    #[arg(long)]
    release: bool,
}

fn main() -> Result<(), Error> {
    let cmd = Command::parse();

    let mut sh = Shell::new()?;
    match cmd {
        Command::Check => check(&mut sh)?,
        Command::Build(opts) => build(&mut sh, opts)?,
        Command::Run(opts) => run(&mut sh, opts)?,
    }
    Ok(())
}

fn run(sh: &mut Shell, opts: Run) -> Result<(), Error> {
    let build_opts = Build {
        release: opts.release,
    };
    build(sh, build_opts)?;
    match opts.release {
        false => {
            cmd!(
                sh,
                "cargo run --bin host -- target/wasm32-wasip2/debug/guest.wasm hello"
            )
            .run()?;
        }
        true => {
            cmd!(
                sh,
                "cargo run --bin host --release -- target/wasm32-wasip2/release/guest.wasm hello"
            )
            .run()?;
        }
    };
    Ok(())
}

fn check(sh: &mut Shell) -> Result<(), Error> {
    {
        let _guard = sh.push_dir("crates/guest");
        cmd!(sh, "cargo component check").run()?;
    }

    {
        let _guard = sh.push_dir("crates/host");
        cmd!(sh, "cargo check").run()?;
    }
    Ok(())
}

fn build(sh: &mut Shell, opts: Build) -> Result<(), Error> {
    {
        let _guard = sh.push_dir("crates/guest");
        match opts.release {
            false => cmd!(sh, "cargo component build --target=wasm32-wasip2").run()?,
            true => cmd!(sh, "cargo component build --target=wasm32-wasip2 --release").run()?,
        }
    }

    {
        let _guard = sh.push_dir("crates/host");
        match opts.release {
            false => cmd!(sh, "cargo build").run()?,
            true => cmd!(sh, "cargo build --release").run()?,
        }
    }
    Ok(())
}
