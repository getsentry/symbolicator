//! Tool to split a WASM file into a binary and debug companion file.

#![warn(missing_docs, missing_debug_implementations, clippy::all)]

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;

use argh::FromArgs;
use uuid::Uuid;
use wasmbin::builtins::Blob;
use wasmbin::sections::{CustomSection, RawCustomSection, Section};
use wasmbin::Module;

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
#[derive(FromArgs, Debug)]
pub struct Cli {
    /// path to the wasm file
    #[argh(positional)]
    input: PathBuf,
    /// path to the output wasm file.
    ///
    /// If not provided the same file is modified in place.
    #[argh(option, short = 'o', long = "out")]
    output: Option<PathBuf>,
    /// path to the output debug wasm file.
    ///
    /// If not provided the debug data stays in the input file.
    #[argh(option, short = 'd', long = "debug-out")]
    debug_output: Option<PathBuf>,
    /// strip the file of debug info.
    #[argh(switch, long = "strip")]
    strip: bool,
    /// do not print the build id.
    #[argh(switch, short = 'q', long = "quiet")]
    quiet: bool,
    /// explicit build id to provide
    #[argh(option)]
    build_id: Option<Uuid>,
}

fn load_custom_section(section: &Section) -> Option<(&str, &[u8])> {
    if let Some(Ok(CustomSection::Other(ref raw))) =
        section.try_as::<CustomSection>().map(|x| x.try_contents())
    {
        Some((&raw.name, &raw.data))
    } else {
        None
    }
}

/// Returns `true` if this section should be stripped.
fn is_strippable_section(section: &Section) -> bool {
    load_custom_section(section).map_or(false, |(name, _)| name.starts_with(".debug_"))
}

fn main() -> Result<(), anyhow::Error> {
    let cli: Cli = argh::from_env();

    let mut module = Module::decode_from(BufReader::new(File::open(&cli.input)?))?;
    let mut build_id = None;
    let mut should_write_main_module = false;

    // try to see if we already have a build ID we can use.  If we already have
    // one we do not need to write a new one.
    for section in &module.sections {
        if let Some((name, data)) = load_custom_section(section) {
            if name == "build_id" {
                build_id = Some(data.to_owned());
                break;
            }
        }
    }

    // if we do have to build a new one, just roll a random uuid v4 as build id.
    let build_id = build_id.unwrap_or_else(|| {
        let new_id = Uuid::new_v4().as_bytes().to_vec();
        should_write_main_module = true;
        module
            .sections
            .push(Section::Custom(Blob::from(CustomSection::Other(
                RawCustomSection {
                    name: "build_id".to_string(),
                    data: new_id.clone(),
                },
            ))));
        new_id
    });

    // split dwarf data out if needed into a separate file.
    if let Some(debug_output) = cli.debug_output {
        // note that this actually copies the entire original file over after
        // adding the build ID.  The reason for this is that we can only deal
        // with WASM files if the code section offset can be calculated.  This
        // means we want to retain the original code section.
        //
        // That said, this limitation might actually turn out to be a good thing.
        // On other platforms we also generally require that we get access to
        // the binary for eh_frame and friends.
        module.encode_into(BufWriter::new(File::create(debug_output)?))?;
    }

    // do we want to strip debug data from main file?
    if cli.strip {
        module
            .sections
            .retain(|section| !is_strippable_section(section));
        should_write_main_module = true;
    }

    // main module
    if should_write_main_module {
        let output = cli.output.as_ref().unwrap_or(&cli.input);
        module.encode_into(BufWriter::new(File::create(output)?))?;
    }

    // always print the build id.
    if !cli.quiet {
        println!("{}", hex::encode(&build_id));
    }

    Ok(())
}
