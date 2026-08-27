//! `rawkit` — the headless face of the engine.
//!
//! The GUI is the product, but the CLI is what CI, the golden harness and the
//! (later) Python lab talk to. Anything the app can do without a window belongs
//! here first: it makes the behaviour scriptable and, more usefully, testable on
//! three operating systems without a display attached.

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rawkit", version, about = "RAW editor — headless tools")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the `EditState` JSON Schema.
    ///
    /// Shared artifact #1: serde defines it here, and everything else — the
    /// Python lab in v2, any external tooling — derives from this output rather
    /// than from a second hand-written copy that can drift.
    Schema,

    /// Print the render pipeline, stage by stage, with each stage's light
    /// domain. The order is a compile-time fact in `rawkit-engine`; this prints
    /// it so a human can check it against the design without reading Rust.
    Stages,

    /// Report the GPU adapter this machine would render on.
    ///
    /// Deliberately trivial and deliberately present from day one: the v1
    /// invariant is that all three OSes render identically, so knowing exactly
    /// which backend and driver produced a given render is the first thing any
    /// cross-platform divergence report needs.
    Gpu,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Schema => {
            let schema = rawkit_editstate::EditState::json_schema();
            println!("{}", serde_json::to_string_pretty(&schema)?);
        }
        Command::Stages => {
            for (i, stage) in rawkit_engine::Stage::ALL.iter().enumerate() {
                let preview = if stage.runs_in_preview() {
                    "preview+export"
                } else {
                    "export only"
                };
                println!(
                    "{:>2}. {:<20} {:<16} {}",
                    i + 1,
                    format!("{stage:?}"),
                    format!("{:?}", stage.domain()),
                    preview
                );
            }
        }
        Command::Gpu => {
            let gpu = rawkit_engine::Gpu::new()?;
            let info = &gpu.adapter_info;
            println!("adapter : {}", info.name);
            println!("backend : {:?}", info.backend);
            println!("type    : {:?}", info.device_type);
            println!("driver  : {} {}", info.driver, info.driver_info);
        }
    }
    Ok(())
}
