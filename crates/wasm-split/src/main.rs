//! Tool to split a WASM file into a binary and debug companion file.

#![warn(missing_docs, missing_debug_implementations, clippy::all)]

use clap::Parser;
use std::path::PathBuf;
use uuid::Uuid;
use wasm_split::{SplitOptions, split};

/// Adds build IDs to wasm files.
///
/// This tool can both add missing build IDs and split a WASM file
/// into two: a main binary and a debug companion file.  The debug
/// companion file will contain all sections of the original file.
/// This is necessary as DWARF processing requires knowing the
/// location of all sections (specially the code section) to
/// calculate offsets.
///
/// This prints the embedded build_id in hexadecimal format to stdout.
#[derive(Debug, Parser)]
#[command(version)]
pub struct Cli {
    /// path to the wasm file
    input: PathBuf,
    /// path to the output wasm file.
    ///
    /// If not provided the same file is modified in place.
    #[arg(short, long)]
    out: Option<PathBuf>,
    /// path to the output debug wasm file.
    ///
    /// If not provided the debug data stays in the input file.
    #[arg(short, long)]
    debug_out: Option<PathBuf>,
    /// strip the file of debug info.
    #[arg(long)]
    strip: bool,
    /// strip the file of symbol names.
    #[arg(long)]
    strip_names: bool,
    /// do not print the build id.
    #[arg(short, long)]
    quiet: bool,
    /// explicit build id to provide
    #[arg(long)]
    build_id: Option<Uuid>,
    /// URL for browsers to fetch the separate dwarf debug symbol file
    #[arg(long)]
    external_dwarf_url: Option<String>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let quiet = cli.quiet;

    let result = split(SplitOptions {
        input: cli.input,
        out: cli.out,
        debug_out: cli.debug_out,
        strip: cli.strip,
        strip_names: cli.strip_names,
        build_id: cli.build_id,
        external_dwarf_url: cli.external_dwarf_url,
    })?;

    // always print the build id.
    if !quiet {
        println!("{}", hex::encode(result.build_id));
    }

    Ok(())
}
