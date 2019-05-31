use std::fs;
use std::io;
use std::path::PathBuf;

use console::style;
use crossbeam::{self, channel};
use failure::Error;
use rayon::prelude::*;
use rayon::ThreadPoolBuilder;
use structopt::StructOpt;
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, FileFormat};
use walkdir::WalkDir;
use zip::ZipArchive;
use zstd::stream::copy_encode;

/// Sorts debug symbols into the right structure for symbolicator.
#[derive(PartialEq, Eq, PartialOrd, Ord, StructOpt, Debug)]
struct Cli {
    /// Path to the output folder structure
    #[structopt(long = "output", short = "o", value_name = "PATH")]
    pub output: PathBuf,

    /// If enabled debug symbols will be zstd compressed (repeat to increase compression)
    #[structopt(
        long = "compress",
        short = "z",
        multiple = true,
        parse(from_occurrences),
        raw(takes_value = "false")
    )]
    pub compression_level: usize,

    /// Controls how many threads should be used for conversion.
    #[structopt(long = "threads", short = "t", value_name = "NUM")]
    pub threads: Option<usize>,

    /// If enabled output will be suppressed
    #[structopt(long = "quiet", short = "q")]
    pub quiet: bool,

    /// Path to input files.
    #[structopt(index = 1)]
    pub input: Vec<PathBuf>,
}

fn execute() -> Result<(), Error> {
    let cli = Cli::from_args();
    let compression_level = match cli.compression_level {
        0 => 0,
        1 => 3,
        2 => 10,
        3 => 19,
        _ => 22,
    };

    if let Some(threads) = cli.threads {
        ThreadPoolBuilder::new()
            .num_threads(threads.max(2))
            .build_global()
            .unwrap();
    }

    let mut reader_rv = None;
    let mut processor_rv = None;

    rayon::scope(|s| {
        let (tx, rx) = channel::bounded::<(ByteView<'static>, String)>(100);
        s.spawn(|_| {
            reader_rv = Some(
                cli.input
                    .par_iter()
                    .try_for_each(|path| -> Result<(), Error> {
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
                                    let name =
                                        zip_file.name().rsplit('/').next().unwrap().to_string();
                                    let bv = ByteView::read(zip_file)?;
                                    if Archive::peek(&bv) != FileFormat::Unknown {
                                        tx.send((bv, name)).unwrap();
                                    }
                                }

                            // object file directly
                            } else if Archive::peek(&bv) != FileFormat::Unknown {
                                tx.send((
                                    bv,
                                    path.file_name().unwrap().to_string_lossy().to_string(),
                                ))
                                .unwrap();
                            }
                        }
                        Ok(())
                    }),
            );
            drop(tx);
        });

        s.spawn(|_| {
            processor_rv = Some(
                rx.into_iter()
                    .par_bridge()
                    .try_fold_with(0usize, |mut rv, (bv, filename)| -> Result<usize, Error> {
                        let archive = Archive::parse(&bv)?;
                        for obj in archive.objects() {
                            let obj = obj?;
                            let debug_id = obj.debug_id().breakpad().to_string().to_lowercase();
                            let dir = cli.output.join(&debug_id[..2]);
                            let new_filename = dir.join(&debug_id[2..]);
                            fs::create_dir_all(dir)?;
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
                            rv += 1;
                        }
                        Ok(rv)
                    })
                    .try_reduce(|| 0, |a, b| Ok(a + b)),
            );
        });
    });

    reader_rv.unwrap()?;
    let total = processor_rv.unwrap()?;

    if !cli.quiet {
        println!();
        println!("Done: sorted {} debug files", style(total).yellow().bold());
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
