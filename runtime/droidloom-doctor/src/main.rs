//! Command-line frontend for the read-only Droidloom capability probe.

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, ValueEnum};
use droidloom_doctor::{ProbeOptions, probe, report_json};

#[derive(Clone, Copy, Debug, ValueEnum)]
enum OutputFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(
    name = "droidloom-doctor",
    version,
    about = "Prove Droidloom host capabilities without changing the host"
)]
struct Cli {
    /// Report representation.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    format: OutputFormat,

    /// Pretty-print JSON output.
    #[arg(long, requires = "format")]
    pretty: bool,

    /// Validate a packaged Droidloom image manifest too.
    #[arg(long, value_name = "PATH")]
    image_manifest: Option<PathBuf>,

    /// Inspect an offline root filesystem instead of `/`.
    #[arg(long, default_value = "/", value_name = "PATH")]
    root: PathBuf,

    /// Architecture override for offline root inspection.
    #[arg(long, value_name = "ARCH")]
    architecture: Option<String>,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let report = probe(&ProbeOptions {
        root: cli.root,
        architecture: cli.architecture,
        image_manifest: cli.image_manifest,
    });

    match cli.format {
        OutputFormat::Human => print!("{}", report.render_human()),
        OutputFormat::Json => match report_json(&report, cli.pretty) {
            Ok(json) => println!("{json}"),
            Err(error) => {
                eprintln!("could not serialize capability report: {error}");
                return ExitCode::from(1);
            }
        },
    }

    if report.ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(2)
    }
}
