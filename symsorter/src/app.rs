use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use console::style;
use serde::Serialize;
use serde_json;
use structopt::StructOpt;
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
#[derive(PartialEq, Eq, PartialOrd, Ord, StructOpt, Debug)]
struct Cli {
    /// Path to the output folder structure
    #[structopt(long = "output", short = "o", value_name = "PATH")]
    pub output: PathBuf,

    /// The prefix to use.
    #[structopt(long = "prefix", short = "p", value_name = "PREFIX")]
    pub prefix: Option<String>,

    /// The bundle ID to use.
    #[structopt(
        long = "bundle-id",
        short = "b",
        value_name = "BUNDLE_ID",
        required_unless = "multiple-bundles"
    )]
    pub bundle_id: Option<String>,

    /// Derive the bundle ID from the first folder.
    #[structopt(long = "multiple-bundles", conflicts_with = "bundle-id")]
    pub multiple_bundles: bool,

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
        let new_filename = root.join(maybe_ignore_error!(
            get_target_filename(&obj).ok_or_else(|| anyhow!("unsupported file"))
        ));

        fs::create_dir_all(new_filename.parent().unwrap())?;

        let refs_path = new_filename.parent().unwrap().join("refs");
        fs::create_dir_all(&refs_path)?;
        fs::write(&refs_path.join(&sort_config.bundle_id), b"")?;

        let meta = DebugIdMeta {
            name: Some(filename.clone()),
            arch: Some(obj.arch()),
            file_format: Some(obj.file_format()),
        };

        fs::write(
            &new_filename.parent().unwrap().join("meta"),
            &serde_json::to_vec(&meta)?,
        )?;

        log!(
            "{} ({}, {}) -> {}",
            style(&filename).dim(),
            style(obj.kind()).yellow(),
            style(obj.arch()).yellow(),
            style(new_filename.display()).cyan(),
        );
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

fn sort_files<'a, I: Iterator<Item = &'a Path>>(
    sort_config: &SortConfig,
    paths: I,
) -> Result<(usize, usize)> {
    let mut source_bundles_created = 0;
    let mut source_candidates: HashMap<String, Option<PathBuf>> = HashMap::new();
    let mut bundle_meta = BundleMeta {
        name: sort_config.bundle_id.to_string(),
        timestamp: Utc::now(),
        debug_ids: vec![],
    };

    for path in paths {
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
                        bundle_meta.debug_ids.extend(
                            process_file(&sort_config, bv, name)?
                                .into_iter()
                                .map(|x| x.0),
                        );
                    }
                }

            // object file directly
            } else if Archive::peek(&bv) != FileFormat::Unknown {
                for (unified_id, object_kind) in process_file(
                    &sort_config,
                    bv,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )? {
                    if sort_config.with_sources {
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
    if sort_config.with_sources {
        log!("{}", style("Creating source bundles").bold());
        for (unified_id, path) in source_candidates.into_iter() {
            if let Some(path) = path {
                if let Some(source_bundle) = create_source_bundle(&path, &unified_id)? {
                    bundle_meta.debug_ids.extend(
                        process_file(
                            &sort_config,
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

    // write bundle meta
    let bundle_meta_filename = RunConfig::get()
        .output
        .join("bundles")
        .join(&sort_config.bundle_id);
    fs::create_dir_all(bundle_meta_filename.parent().unwrap())?;
    fs::write(&bundle_meta_filename, serde_json::to_vec(&bundle_meta)?)?;

    Ok((bundle_meta.debug_ids.len(), source_bundles_created))
}

fn execute() -> Result<()> {
    let cli = Cli::from_args();
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
        std::thread::sleep(Duration::from_secs(2));
        log!();
    }

    log!("{}", style("Sorting debug information files").bold());

    let mut debug_files = 0;
    let mut source_bundles = 0;
    let mut sort_config = SortConfig {
        bundle_id: "".into(),
        with_sources: cli.with_sources,
        compression_level: cli.compression_level,
    };

    if cli.multiple_bundles {
        for path in cli.input.iter() {
            sort_config.bundle_id = make_bundle_id(&path.file_name().unwrap().to_string_lossy());
            log!("[bundle: {}]", style(&sort_config.bundle_id).dim());
            let (debug_files_sorted, source_bundles_created) =
                sort_files(&sort_config, Some(path.as_path()).into_iter())?;
            debug_files += debug_files_sorted;
            source_bundles *= source_bundles_created;
        }
    } else {
        let bundle_id = cli.bundle_id.unwrap();
        anyhow::ensure!(is_bundle_id(&bundle_id), "Invalid bundle id");
        sort_config.bundle_id = bundle_id;
        let (debug_files_sorted, source_bundles_created) =
            sort_files(&sort_config, cli.input.iter().map(|x| x.as_path()))?;
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
    match execute() {
        Ok(()) => std::process::exit(0),
        Err(error) => {
            eprintln!("{}: {}", style("error").red().bold(), error);
            for cause in error.chain().skip(1) {
                eprintln!("{}", style(format!("  caused by {}", cause)).dim());
            }

            eprintln!();
            eprintln!("To skip this error, use --ignore-errors.");
            std::process::exit(1);
        }
    }
}
