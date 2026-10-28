//! Auditable Droidloom supervisor planning CLI.

#![forbid(unsafe_code)]

use std::fs;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use droidloom_supervisor::CellSpec;
use droidloom_supervisor::development::{
    enter_development_cell, run_development_cell, share_storage_into_cell,
    validate_development_inputs,
};
use droidloom_supervisor::linux_plan::build_linux_plan;

const MAX_SPEC_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "droidloom-supervisor", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Render the complete construction and reverse teardown plan as JSON.
    Plan {
        /// Cell specification JSON.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Validate the inputs for the minimal first-boot cell.
    DevelopmentValidate {
        /// Cell specification JSON.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Start the minimal first-boot cell and attach to Android init logs.
    DevelopmentRun {
        /// Cell specification JSON.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Copy stopped legacy Android images into new direct host directories.
    MigrateStorage {
        /// Existing data_dir containing detached data.img and metadata.img.
        #[arg(long)]
        source: PathBuf,
        /// New data_dir; must not exist. Configuration is not changed.
        #[arg(long)]
        destination: PathBuf,
        #[arg(long, hide = true)]
        private_worker: bool,
    },
    /// Private namespace re-entry used only by `development-run`.
    #[command(hide = true)]
    DevelopmentEnter {
        /// Cell specification JSON.
        #[arg(long)]
        spec: PathBuf,
    },
    /// Private step of droidloomd: shares the folders into the running cell.
    #[command(hide = true)]
    DevelopmentShare {
        /// Cell specification JSON.
        #[arg(long)]
        spec: PathBuf,
        /// Host PID of the cell's Android init.
        #[arg(long)]
        init: u32,
    },
}

fn main() {
    if let Err(error) = run() {
        eprintln!("droidloom-supervisor: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    match Cli::parse().command {
        Command::Plan { spec } => {
            let spec = load_spec(&spec)?;
            let plan = build_linux_plan(&spec)?;
            println!("{}", serde_json::to_string_pretty(&plan)?);
        }
        Command::DevelopmentValidate { spec } => {
            let spec = load_spec(&spec)?;
            validate_development_inputs(&spec)?;
            println!("development boot inputs are valid");
        }
        Command::DevelopmentRun { spec } => {
            let cell = load_spec(&spec)?;
            run_development_cell(&spec, &cell)?;
        }
        Command::MigrateStorage {
            source,
            destination,
            private_worker,
        } => {
            droidloom_supervisor::storage_migration::migrate(
                &source,
                &destination,
                private_worker,
            )?;
        }
        Command::DevelopmentEnter { spec } => {
            let spec = load_spec(&spec)?;
            enter_development_cell(&spec)?;
        }
        Command::DevelopmentShare { spec, init } => {
            let spec = load_spec(&spec)?;
            share_storage_into_cell(&spec, init)?;
        }
    }
    Ok(())
}

fn load_spec(path: &PathBuf) -> Result<CellSpec, Box<dyn std::error::Error>> {
    let metadata = fs::metadata(path)?;
    if metadata.len() > MAX_SPEC_BYTES {
        return Err(format!(
            "cell specification is {} bytes; maximum is {MAX_SPEC_BYTES}",
            metadata.len()
        )
        .into());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
