use std::collections::HashMap;
use std::fs;
use std::io;
use std::ops::Deref;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow};
use chrono::{DateTime, Utc};
use clap::Parser;
use console::style;
use rayon::prelude::*;
use serde::Serialize;
use symbolic::common::{Arch, ByteView};
use symbolic::debuginfo::{Archive, FileFormat, ObjectKind};
use walkdir::WalkDir;
use zip::ZipArchive;
use zstd::stream::copy_encode;

use crate::config::{RunConfig, SortConfig};
use crate::utils::{
    create_source_bundle, get_target_filename, get_unified_id, is_bundle_id, make_bundle_id,
};

/// Sorts debug symbols into the right structure for symbolicator.
#[derive(PartialEq, Eq, PartialOrd, Ord, Parser, Debug)]
#[command(version)]
struct Cli {
    /// Path to the output folder structure
    #[arg(long = "output", short = 'o', value_name = "PATH")]
    pub output: PathBuf,

    /// The prefix to use.
    #[arg(long = "prefix", short = 'p', value_name = "PREFIX")]
    pub prefix: Option<String>,

    /// The bundle ID to use.
    #[arg(long = "bundle-id", short = 'b', value_name = "BUNDLE_ID")]
    pub bundle_id: Option<String>,

    /// Derive the bundle ID from the first folder.
    #[arg(long = "multiple-bundles", conflicts_with = "bundle_id")]
    pub multiple_bundles: bool,

    /// If enable the system will attempt to create source bundles
    #[arg(long = "with-sources")]
    pub with_sources: bool,

    /// If enabled debug symbols will be zstd compressed (repeat to increase compression)
    #[arg(
        long = "compress",
        short = 'z',
        action = clap::ArgAction::Count,
        value_parser = clap::value_parser!(u8)
    )]
    pub compression_level: u8,

    /// Ignore broken archives.
    #[arg(long = "ignore-errors", short = 'I')]
    pub ignore_errors: bool,

    /// If enabled output will be suppressed
    #[arg(long = "quiet", short = 'q')]
    pub quiet: bool,

    /// Path to input files.
    #[arg(index = 1)]
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

fn process_file(
    sort_config: &SortConfig,
    bv: ByteView<'static>,
    filename: String,
) -> Result<Vec<(String, ObjectKind)>> {
    let mut rv = vec![];

    macro_rules! maybe_ignore_error {
        ($expr:expr) => {
            match $expr {
                Ok(value) => value,
                Err(err) => {
                    if RunConfig::get().ignore_errors {
                        eprintln!(
                            "{}: ignored error {} ({})",
                            style("error").red().bold(),
                            err,
                            style(filename).cyan(),
                        );
                        return Ok(rv);
                    } else {
                        return Err(err).context(format!("failed to process file {}", filename));
                    }
                }
            }
        };
    }

    let compression_level = match sort_config.compression_level {
        0 => 0,
        1 => 3,
        2 => 10,
        3 => 19,
        _ => 22,
    };
    let archive = maybe_ignore_error!(
        Archive::parse(&bv).map_err(|e| anyhow!("failed to parse archive {}", e))
    );
    let root = &RunConfig::get().output;

    for obj in archive.objects() {
        let obj = maybe_ignore_error!(obj.map_err(|e| anyhow!(e)));
        if !matches!(
            obj.kind(),
            ObjectKind::Executable | ObjectKind::Library | ObjectKind::Debug | ObjectKind::Sources
        ) {
            continue;
        }
        let new_filename = root.join(maybe_ignore_error!(get_target_filename(&obj)));

        fs::create_dir_all(new_filename.parent().unwrap())?;

        if let Some(bundle_id) = &sort_config.bundle_id {
            let refs_path = new_filename.parent().unwrap().join("refs");
            fs::create_dir_all(&refs_path)?;
            fs::write(refs_path.join(bundle_id), b"")?;
        }

        let meta = DebugIdMeta {
            name: Some(filename.clone()),
            arch: Some(obj.arch()),
            file_format: Some(obj.file_format()),
        };

        fs::write(
            new_filename.with_extension("meta"),
            serde_json::to_vec(&meta)?,
        )?;

        log!(
            "{} ({}, {}) -> {}",
            style(&filename).dim(),
            style(obj.kind()).yellow(),
            style(obj.arch()).yellow(),
            style(new_filename.display()).cyan(),
        );

        let mut out = match fs::File::create_new(&new_filename) {
            Ok(out) => out,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                eprintln!(
                    "{}: File {} already exists, you seem to have duplicate debug files for ID {}.\n\
                    Skipping {filename}.",
                    style("WARNING").red().bold(),
                    new_filename.display(),
                    get_unified_id(&obj)?,
                );
                return Ok(vec![]);
            }
            Err(e) => return Err(e.into()),
        };

        if compression_level > 0 {
            copy_encode(obj.data(), &mut out, compression_level)?;
        } else {
            io::copy(&mut obj.data(), &mut out)?;
        }
        let unified_id = maybe_ignore_error!(get_unified_id(&obj));
        rv.push((unified_id, obj.kind()));
    }

    Ok(rv)
}

fn process_zip_archive(
    sort_config: &SortConfig,
    bv: ByteView,
    zip_file_name: &str,
    debug_ids: &Mutex<Vec<String>>,
) -> Result<()> {
    let result: Result<()> = (|| {
        let mut zip = ZipArchive::new(io::Cursor::new(bv))?;
        for index in 0..zip.len() {
            let zip_file = zip.by_index(index)?;
            let name = zip_file.name().rsplit('/').next().unwrap().to_string();
            let bv = ByteView::read(zip_file)?;
            if Archive::peek(&bv) != FileFormat::Unknown {
                debug_ids.lock().unwrap().extend(
                    process_file(sort_config, bv, name)?
                        .into_iter()
                        .map(|x| x.0),
                );
            }
        }
        Ok(())
    })();
    if RunConfig::get().ignore_errors {
        if let Err(err) = &result {
            eprintln!(
                "{}: ignored error {} ({})",
                style("error").red().bold(),
                err,
                style(zip_file_name).cyan(),
            );
        }
        Ok(())
    } else {
        result.with_context(|| format!("failed to process file {zip_file_name}"))
    }
}

fn sort_files(sort_config: &SortConfig, paths: Vec<PathBuf>) -> Result<(usize, usize)> {
    let mut source_bundles_created = 0;
    let source_candidates = Mutex::new(HashMap::<String, Option<PathBuf>>::new());
    let debug_ids = Mutex::new(Vec::new());

    log!("{}", style("Sorting debug information files").bold());

    paths
        .into_iter()
        .flat_map(WalkDir::new)
        .filter_map(Result::ok)
        .filter(|entry| entry.metadata().is_ok_and(|x| x.is_file()))
        .par_bridge()
        .map(|entry| {
            let path = entry.path();
            let bv = match ByteView::open(path) {
                Ok(bv) => bv,
                Err(_) => return Ok(()),
            };

            // zip archive
            if bv.get(..2) == Some(b"PK") {
                process_zip_archive(sort_config, bv, path.to_string_lossy().deref(), &debug_ids)?;

            // object file directly
            } else if Archive::peek(&bv) != FileFormat::Unknown {
                for (unified_id, object_kind) in process_file(
                    sort_config,
                    bv,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )? {
                    if sort_config.with_sources {
                        let mut source_candidates = source_candidates.lock().unwrap();
                        if object_kind == ObjectKind::Sources {
                            source_candidates.insert(unified_id.clone(), None);
                        } else if object_kind == ObjectKind::Debug
                            && !source_candidates.contains_key(&unified_id)
                        {
                            source_candidates.insert(unified_id.clone(), Some(path.to_path_buf()));
                        }
                    }
                    debug_ids.lock().unwrap().push(unified_id);
                }
            }

            Ok(())
        })
        .collect::<Result<Vec<_>>>()?;

    if sort_config.with_sources {
        log!("{}", style("Creating source bundles").bold());
        source_bundles_created = source_candidates
            .into_inner()
            .unwrap()
            .into_par_iter()
            .filter_map(|(id, path)| Some((id, path?)))
            .map(|(unified_id, path)| -> Result<usize> {
                let source_bundle = match create_source_bundle(&path, &unified_id)? {
                    Some(source_bundle) => source_bundle,
                    None => return Ok(0),
                };

                let processed_objects = process_file(
                    sort_config,
                    source_bundle,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )?;

                debug_ids
                    .lock()
                    .unwrap()
                    .extend(processed_objects.into_iter().map(|x| x.0));

                Ok(1)
            })
            .reduce(|| Ok(0), |sum, res| Ok(sum? + res?))?
    }

    let debug_ids = debug_ids.into_inner()?;
    let num_debug_ids = debug_ids.len();

    if let Some(bundle_id) = &sort_config.bundle_id {
        log!("{}", style("Writing bundle meta data").bold());

        let bundle_meta = BundleMeta {
            name: bundle_id.clone(),
            timestamp: Utc::now(),
            debug_ids,
        };

        let bundle_meta_filename = RunConfig::get().output.join("bundles").join(bundle_id);
        fs::create_dir_all(bundle_meta_filename.parent().unwrap())?;
        fs::write(&bundle_meta_filename, serde_json::to_vec(&bundle_meta)?)?;
    }

    Ok((num_debug_ids, source_bundles_created))
}

fn execute(cli: Cli) -> Result<()> {
    RunConfig::configure(|cfg| {
        cfg.ignore_errors = cli.ignore_errors;
        cfg.quiet = cli.quiet;
        cfg.output = if let Some(ref prefix) = cli.prefix {
            cli.output.join(prefix)
        } else {
            cli.output.clone()
        };
    });

    if cli.compression_level == 0 {
        log!(
            "{}: {}",
            style("WARNING").bold().red(),
            "No compression used. Consider to pass -zz."
        );
        log!();
    }

    let mut debug_files = 0;
    let mut source_bundles = 0;
    let mut sort_config = SortConfig {
        bundle_id: None,
        with_sources: cli.with_sources,
        compression_level: cli.compression_level,
    };

    if cli.multiple_bundles {
        for path in cli.input.into_iter() {
            let bundle_id = make_bundle_id(&path.file_name().unwrap().to_string_lossy());
            log!("[bundle: {}]", style(&bundle_id).dim());
            sort_config.bundle_id = Some(bundle_id);
            let (debug_files_sorted, source_bundles_created) =
                sort_files(&sort_config, vec![path])?;
            debug_files += debug_files_sorted;
            source_bundles *= source_bundles_created;
        }
    } else {
        if let Some(bundle_id) = cli.bundle_id {
            anyhow::ensure!(is_bundle_id(&bundle_id), "Invalid bundle id");
            sort_config.bundle_id = Some(bundle_id);
        }
        let (debug_files_sorted, source_bundles_created) = sort_files(&sort_config, cli.input)?;
        debug_files += debug_files_sorted;
        source_bundles *= source_bundles_created;
    }

    log!();
    log!("{}", style("Done.").bold());
    log!("Sorted {} debug files", style(debug_files).yellow().bold());
    if cli.with_sources {
        log!(
            "Created {} source bundles",
            style(source_bundles).yellow().bold()
        );
    }

    Ok(())
}

pub fn main() -> ! {
    let cli = Cli::parse();
    let ignore_errors = cli.ignore_errors;
    match execute(cli) {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("{}: {}", style("error").red().bold(), error);
            for cause in error.chain().skip(1) {
                eprintln!("{}", style(format!("  caused by {cause}")).dim());
            }

            if !ignore_errors {
                eprintln!();
                eprintln!("To skip this error, use --ignore-errors.");
            }
            std::process::exit(1);
        }
    }
}
