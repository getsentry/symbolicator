//! Tool to split a WASM file into a binary and debug companion file.

#![warn(
    missing_docs,
    missing_debug_implementations,
    unused_crate_dependencies,
    clippy::all
)]

use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::PathBuf;
use structopt::StructOpt;
use uuid::Uuid;
use wasmbin::builtins::Blob;
use wasmbin::io::Encode;
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
#[derive(Debug, StructOpt)]
pub struct Cli {
    /// path to the wasm file
    input: PathBuf,
    /// path to the output wasm file.
    ///
    /// If not provided the same file is modified in place.
    #[structopt(short, long)]
    out: Option<PathBuf>,
    /// path to the output debug wasm file.
    ///
    /// If not provided the debug data stays in the input file.
    #[structopt(short, long)]
    debug_out: Option<PathBuf>,
    /// strip the file of debug info.
    #[structopt(long)]
    strip: bool,
    /// strip the file of symbol names.
    #[structopt(long)]
    strip_names: bool,
    /// do not print the build id.
    #[structopt(short, long)]
    quiet: bool,
    /// explicit build id to provide
    #[structopt(long)]
    build_id: Option<Uuid>,
    /// URL for browsers to fetch the separate dwarf debug symbol file
    #[structopt(long)]
    external_dwarf_url: Option<String>,
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
fn is_strippable_section(section: &Section, strip_names: bool) -> bool {
    fn custom_section_name(section: &Section) -> Option<&str> {
        Some(match section.try_as()?.try_contents().ok()? {
            CustomSection::Name(_) => "name",
            CustomSection::Producers(_) => "producers",
            CustomSection::Other(s) => &s.name,
        })
    }

    match custom_section_name(section) {
        Some("name") => strip_names,
        Some(other) if other.starts_with(".debug_") => true,
        _ => false,
    }
}

fn main() -> Result<(), anyhow::Error> {
    let cli = Cli::from_args();

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

    // if we do have to build a new one, use the one from the command line or fall back to
    // a random uuid v4 as build id.
    let build_id = build_id.unwrap_or_else(|| {
        let new_id = cli
            .build_id
            .unwrap_or_else(Uuid::new_v4)
            .as_bytes()
            .to_vec();
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
    if let Some(debug_output) = cli.debug_out.as_ref() {
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
        let strip_names = cli.strip_names;
        module
            .sections
            .retain(|section| !is_strippable_section(section, strip_names));
        should_write_main_module = true;
    }

    // If the debug file path is set, resolve the filename (ie. /some/path/to/foo.debug.wasm -> foo.debug.wasm)
    let debug_file_name = cli
        .debug_out
        .as_ref()
        .and_then(|name| name.file_name())
        .and_then(|name| name.to_str())
        .map(|name| name.to_string());

    // Use the command line flag if set, but fallback to the debug file name if that is set.
    // This is a reasonable default, as the filename on its own will resolve as a path relative to the main wasm file.
    // Emscripten falls back in the same way: https://github.com/emscripten-core/emscripten/pull/12549
    let resolved_external_dwarf_url = cli.external_dwarf_url.or(debug_file_name);

    if let Some(external_dwarf_url) = resolved_external_dwarf_url {
        should_write_main_module = true;

        // From the wasm spec, the URL is encoded as bytes, and prefixed with a varint encoding of a u32 size
        // https://github.com/WebAssembly/tool-conventions/blob/08bacbed/Debugging.md#external-dwarf
        // Emscripten: https://github.com/emscripten-core/emscripten/blob/4eefe273/tools/building.py#L1200
        // We use the `wasmbin::io::Encode` trait, as it will handle serializing a string as a Vec<u8> with
        // an LEB128 prefix.
        let data_vec = &mut Vec::new();
        external_dwarf_url.encode(data_vec).unwrap();

        module
            .sections
            .push(Section::Custom(Blob::from(CustomSection::Other(
                RawCustomSection {
                    name: "external_debug_info".to_string(),
                    data: data_vec.to_vec(),
                },
            ))));
    }

    // main module
    if should_write_main_module {
        let output = cli.out.as_ref().unwrap_or(&cli.input);
        module.encode_into(BufWriter::new(File::create(output)?))?;
    }

    // always print the build id.
    if !cli.quiet {
        println!("{}", hex::encode(&build_id));
    }

    Ok(())
}
