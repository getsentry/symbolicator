use std::fs;
use std::io;
use std::path::PathBuf;

use console::style;
use crossbeam::{self, channel};
use failure::Error;
use rayon::prelude::*;
use structopt::StructOpt;
use symbolic::common::ByteView;
use symbolic::debuginfo::{Archive, FileFormat};
use walkdir::WalkDir;
use zstd::stream::copy_encode;

#[derive(PartialEq, Eq, PartialOrd, Ord, StructOpt, Debug)]
struct Cli {
    /// Path to the output folder structure
    #[structopt(long = "output", short = "o", value_name = "PATH")]
    pub output: PathBuf,

    /// If enabled debug symbols will be zstd compressed.
    #[structopt(long = "compress", short = "z")]
    pub compress: bool,

    /// If enabled output will be suppressed
    #[structopt(long = "quiet", short = "q")]
    pub quiet: bool,

    /// Path to input files.
    #[structopt(index = 1)]
    pub input: Vec<PathBuf>,
}

fn execute() -> Result<(), Error> {
    let cli = Cli::from_args();

    let (tx, rx) = channel::bounded::<(ByteView<'static>, String)>(30);

    let mut reader_rv = None;
    let mut processor_rv = None;
    rayon::scope(|s| {
        s.spawn(|_| {
            reader_rv = Some(
                cli.input
                    .par_iter()
                    .map(|path| -> Result<(), Error> {
                        for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
                            if !entry.metadata()?.is_file() {
                                continue;
                            }

                            let path = entry.path();
                            let bv = ByteView::open(path)?;
                            if Archive::peek(&bv) != FileFormat::Unknown {
                                tx.send((
                                    bv,
                                    path.file_name().unwrap().to_string_lossy().to_string(),
                                ))
                                .unwrap();
                            }
                        }
                        Ok(())
                    })
                    .collect::<Result<Vec<_>, _>>(),
            );
            drop(tx);
        });

        s.spawn(|_| {
            processor_rv = Some(
                rx.into_iter()
                    .par_bridge()
                    .map(|(bv, filename)| -> Result<usize, Error> {
                        let archive = Archive::parse(&bv)?;
                        let mut rv = 0;
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
                            if cli.compress {
                                copy_encode(obj.data(), &mut out, 3)?;
                            } else {
                                io::copy(&mut obj.data(), &mut out)?;
                            }
                            rv += 1;
                        }
                        Ok(rv)
                    })
                    .collect::<Result<Vec<_>, _>>(),
            );
        });
    });

    reader_rv.unwrap()?;
    let total = processor_rv.unwrap()?.into_iter().sum::<usize>();

    if !cli.quiet {
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
