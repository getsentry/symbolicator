use std::fs;
use std::io;
use std::path::PathBuf;

use console::style;
use failure::{err_msg, Error};
use chrono::{DateTime, Utc};
use structopt::StructOpt;
use symbolic::common::{ByteView, Arch};
use symbolic::debuginfo::{Archive, FileFormat, Object, ObjectKind};
use walkdir::WalkDir;
use zip::ZipArchive;
use zstd::stream::copy_encode;
use serde::Serialize;
use serde_json;

/// File metadata

/// Sorts debug symbols into the right structure for symbolicator.
#[derive(PartialEq, Eq, PartialOrd, Ord, StructOpt, Debug)]
struct Cli {
    /// Path to the output folder structure
    #[structopt(long = "output", short = "o", value_name = "PATH")]
    pub output: PathBuf,

    /// The prefix to use.
    #[structopt(long = "prefix", short = "p", value_name = "PREFIX")]
    pub prefix: String,

    /// The bundle ID to use.
    #[structopt(long = "bundle-id", short = "b", value_name = "BUNDLE_ID")]
    pub bundle_id: String,

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

fn get_unified_id<'a>(obj: &Object) -> String {
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

fn process_file(cli: &Cli, bv: ByteView<'static>, filename: String) -> Result<Vec<String>, Error> {
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
    let root = cli.output.join(&cli.prefix);

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

        fs::write(&new_filename.parent().unwrap().join("meta"), &serde_json::to_vec(&meta)?)?;

        if !cli.quiet {
            println!(
                "{} ({}) -> {}",
                style(&filename).dim(),
                style(obj.arch().to_string()).yellow(),
                style(new_filename.display()).cyan()
            );
        }
        let mut out = fs::File::create(&new_filename)?;

        if compression_level > 0 {
            copy_encode(obj.data(), &mut out, compression_level)?;
        } else {
            io::copy(&mut obj.data(), &mut out)?;
        }
        rv.push(get_unified_id(&obj));
    }

    Ok(rv)
}

fn execute() -> Result<(), Error> {
    let cli = Cli::from_args();
    let mut bundle_meta = BundleMeta {
        name: cli.bundle_id.to_string(),
        timestamp: Utc::now(),
        debug_ids: vec![],
    };

    for path in cli.input.iter() {
        for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
            if !entry.metadata()?.is_file() {
                continue;
            }

            let path = entry.path();
            let bv = ByteView::open(path)?;

            // zip archive
            if bv.get(..2) == Some(b"PK") {
                let mut zip = ZipArchive::new(io::Cursor::new(&bv[..]))?;
                for index in 0..zip.len() {
                    let zip_file = zip.by_index(index)?;
                    let name = zip_file.name().rsplit('/').next().unwrap().to_string();
                    let bv = ByteView::read(zip_file)?;
                    if Archive::peek(&bv) != FileFormat::Unknown {
                        bundle_meta.debug_ids.extend(process_file(&cli, bv, name)?);
                    }
                }

            // object file directly
            } else if Archive::peek(&bv) != FileFormat::Unknown {
                bundle_meta.debug_ids.extend(process_file(
                    &cli,
                    bv,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )?);
            }
        }
    }

    let root = cli.output.join(&cli.prefix);
    let bundle_meta_filename = root.join("bundles").join(&cli.bundle_id);
    fs::create_dir_all(bundle_meta_filename.parent().unwrap())?;
    fs::write(&bundle_meta_filename, serde_json::to_vec(&bundle_meta)?)?;

    if !cli.quiet {
        println!();
        println!("Done: sorted {} debug files", style(bundle_meta.debug_ids.len()).yellow().bold());
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
