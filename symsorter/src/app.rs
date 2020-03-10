use std::collections::HashMap;
use std::fs;
use std::io::{self, Cursor};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use console::style;
use failure::{err_msg, Error};
use serde::Serialize;
use serde_json;
use structopt::StructOpt;
use symbolic::common::{Arch, ByteView};
use symbolic::debuginfo::sourcebundle::SourceBundleWriter;
use symbolic::debuginfo::{Archive, FileFormat, Object, ObjectKind};
use walkdir::WalkDir;
use zip::ZipArchive;
use zstd::stream::copy_encode;

/// File metadata

/// Sorts debug symbols into the right structure for symbolicator.
#[derive(PartialEq, Eq, PartialOrd, Ord, StructOpt, Debug)]
struct Cli {
    /// Path to the output folder structure
    #[structopt(long = "output", short = "o", value_name = "PATH")]
    pub output: PathBuf,

    /// The prefix to use.
    #[structopt(long = "prefix", short = "p", value_name = "PREFIX")]
    pub prefix: Option<String>,

    /// The bundle ID to use.
    #[structopt(long = "bundle-id", short = "b", value_name = "BUNDLE_ID")]
    pub bundle_id: String,

    /// If enable the system will attempt to create source bundles
    #[structopt(long = "with-sources")]
    pub with_sources: bool,

    /// If enabled debug symbols will be zstd compressed (repeat to increase compression)
    #[structopt(
        long = "compress",
        short = "z",
        multiple = true,
        parse(from_occurrences),
        takes_value(false)
    )]
    pub compression_level: usize,

    /// Ignore broken archives.
    #[structopt(long = "ignore-errors", short = "I")]
    pub ignore_errors: bool,

    /// If enabled output will be suppressed
    #[structopt(long = "quiet", short = "q")]
    pub quiet: bool,

    /// Path to input files.
    #[structopt(index = 1)]
    pub input: Vec<PathBuf>,
}

#[derive(Serialize)]
pub struct DebugIdMeta {
    pub name: Option<String>,
    pub arch: Option<Arch>,
    pub file_format: Option<FileFormat>,
}

#[derive(Serialize)]
pub struct BundleMeta {
    pub name: String,
    pub timestamp: DateTime<Utc>,
    pub debug_ids: Vec<String>,
}

fn get_unified_id(obj: &Object) -> String {
    if obj.file_format() == FileFormat::Pe || obj.code_id().is_none() {
        obj.debug_id().breakpad().to_string().to_lowercase()
    } else {
        obj.code_id().as_ref().unwrap().as_str().to_string()
    }
}

fn get_target_filename(obj: &Object) -> Option<PathBuf> {
    let id = get_unified_id(obj);
    // match the unified format here.
    let suffix = match obj.kind() {
        ObjectKind::Debug => "debuginfo",
        ObjectKind::Sources => {
            if obj.file_format() == FileFormat::SourceBundle {
                "sourcebundle"
            } else {
                return None;
            }
        }
        ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => "executable",
        _ => return None,
    };
    Some(format!("{}/{}/{}", &id[..2], &id[2..], suffix).into())
}

fn process_file(
    cli: &Cli,
    bv: ByteView<'static>,
    filename: String,
) -> Result<Vec<(String, ObjectKind)>, Error> {
    let mut rv = vec![];

    macro_rules! maybe_ignore_error {
        ($expr:expr) => {
            match $expr {
                Ok(value) => value,
                Err(err) => {
                    if cli.ignore_errors {
                        eprintln!(
                            "{}: ignored error {} ({})",
                            style("error").red().bold(),
                            err,
                            style(filename).cyan(),
                        );
                        return Ok(rv);
                    } else {
                        return Err(err.into());
                    }
                }
            }
        };
    }

    let compression_level = match cli.compression_level {
        0 => 0,
        1 => 3,
        2 => 10,
        3 => 19,
        _ => 22,
    };
    let archive = maybe_ignore_error!(Archive::parse(&bv));
    let root = if let Some(ref prefix) = cli.prefix {
        cli.output.join(prefix)
    } else {
        cli.output.clone()
    };

    for obj in archive.objects() {
        let obj = maybe_ignore_error!(obj);
        let new_filename = root.join(maybe_ignore_error!(
            get_target_filename(&obj).ok_or_else(|| err_msg("unsupported file"))
        ));

        fs::create_dir_all(new_filename.parent().unwrap())?;

        let refs_path = new_filename.parent().unwrap().join("refs");
        fs::create_dir_all(&refs_path)?;
        fs::write(&refs_path.join(&cli.bundle_id), b"")?;

        let meta = DebugIdMeta {
            name: Some(filename.clone()),
            arch: Some(obj.arch()),
            file_format: Some(obj.file_format()),
        };

        fs::write(
            &new_filename.parent().unwrap().join("meta"),
            &serde_json::to_vec(&meta)?,
        )?;

        if !cli.quiet {
            println!(
                "{} ({}, {}) -> {}",
                style(&filename).dim(),
                style(obj.kind()).yellow(),
                style(obj.arch()).yellow(),
                style(new_filename.display()).cyan(),
            );
        }
        let mut out = fs::File::create(&new_filename)?;

        if compression_level > 0 {
            copy_encode(obj.data(), &mut out, compression_level)?;
        } else {
            io::copy(&mut obj.data(), &mut out)?;
        }
        rv.push((get_unified_id(&obj), obj.kind()));
    }

    Ok(rv)
}

fn create_source_bundle(path: &Path, unified_id: &str) -> Result<Option<ByteView<'static>>, Error> {
    let bv = ByteView::open(path)?;
    let archive = Archive::parse(&bv)?;
    for obj in archive.objects() {
        let obj = obj?;
        if get_unified_id(&obj) == unified_id {
            let mut out = Vec::<u8>::new();
            let writer = SourceBundleWriter::start(Cursor::new(&mut out))?;
            if writer.write_object(
                &obj,
                &path.file_name().unwrap().to_string_lossy().to_string(),
            )? {
                return Ok(Some(ByteView::from_vec(out)));
            }
        }
    }
    Ok(None)
}

fn execute() -> Result<(), Error> {
    let cli = Cli::from_args();
    let mut bundle_meta = BundleMeta {
        name: cli.bundle_id.to_string(),
        timestamp: Utc::now(),
        debug_ids: vec![],
    };
    let mut source_candidates: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut source_bundles_created = 0;

    if !cli.quiet {
        println!("{}", style("Sorting debug information files").bold());
    }

    for path in cli.input.iter() {
        for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
            if !entry.metadata().ok().map_or(false, |x| x.is_file()) {
                continue;
            }

            let path = entry.path();
            let bv = match ByteView::open(path) {
                Ok(bv) => bv,
                Err(_) => continue,
            };

            // zip archive
            if bv.get(..2) == Some(b"PK") {
                let mut zip = ZipArchive::new(io::Cursor::new(&bv[..]))?;
                for index in 0..zip.len() {
                    let zip_file = zip.by_index(index)?;
                    let name = zip_file.name().rsplit('/').next().unwrap().to_string();
                    let bv = ByteView::read(zip_file)?;
                    if Archive::peek(&bv) != FileFormat::Unknown {
                        bundle_meta
                            .debug_ids
                            .extend(process_file(&cli, bv, name)?.into_iter().map(|x| x.0));
                    }
                }

            // object file directly
            } else if Archive::peek(&bv) != FileFormat::Unknown {
                for (unified_id, object_kind) in process_file(
                    &cli,
                    bv,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )? {
                    if cli.with_sources {
                        if object_kind == ObjectKind::Sources {
                            source_candidates.insert(unified_id.clone(), None);
                        } else if object_kind == ObjectKind::Debug
                            && !source_candidates.contains_key(&unified_id)
                        {
                            source_candidates.insert(unified_id.clone(), Some(path.to_path_buf()));
                        }
                    }
                    bundle_meta.debug_ids.push(unified_id);
                }
            }
        }
    }

    // we have some sources we want to build
    if cli.with_sources {
        if !cli.quiet {
            println!("{}", style("Creating source bundles").bold());
        }
        for (unified_id, path) in source_candidates.into_iter() {
            if let Some(path) = path {
                if let Some(source_bundle) = create_source_bundle(&path, &unified_id)? {
                    bundle_meta.debug_ids.extend(
                        process_file(
                            &cli,
                            source_bundle,
                            path.file_name().unwrap().to_string_lossy().to_string(),
                        )?
                        .into_iter()
                        .map(|x| x.0),
                    );
                    source_bundles_created += 1;
                }
            }
        }
    }

    let root = if let Some(ref prefix) = cli.prefix {
        cli.output.join(prefix)
    } else {
        cli.output.clone()
    };
    let bundle_meta_filename = root.join("bundles").join(&cli.bundle_id);
    fs::create_dir_all(bundle_meta_filename.parent().unwrap())?;
    fs::write(&bundle_meta_filename, serde_json::to_vec(&bundle_meta)?)?;

    if !cli.quiet {
        println!();
        println!("{}", style("Done.").bold());
        println!(
            "Sorted {} debug files",
            style(bundle_meta.debug_ids.len()).yellow().bold()
        );
        if cli.with_sources {
            println!(
                "Created {} source bundles",
                style(source_bundles_created).yellow().bold()
            );
        }
    }

    Ok(())
}

pub fn main() -> ! {
    match execute() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("error: {}", error);
            std::process::exit(1);
        }
    }
}
