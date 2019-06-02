use std::fmt::Write;
use std::fs;
use std::io;
use std::path::PathBuf;

use console::style;
use failure::Error;
use structopt::StructOpt;
use symbolic::common::{ByteView, DebugId};
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

    /// If enabled output will be suppressed
    #[structopt(long = "quiet", short = "q")]
    pub quiet: bool,

    /// Path to input files.
    #[structopt(index = 1)]
    pub input: Vec<PathBuf>,
}

fn get_target_filename(debug_id: &DebugId) -> PathBuf {
    // Format the UUID as "xxxx/xxxx/xxxx/xxxx/xxxx/xxxxxxxxxxxx"
    let uuid = debug_id.uuid();
    let slice = uuid.as_bytes();
    let mut path = String::with_capacity(37);
    for (i, byte) in slice.iter().enumerate() {
        write!(path, "{:02X}", byte).ok();
        if i % 2 == 1 && i <= 9 {
            path.push('/');
        }
    }
    path.into()
}

fn process_file(cli: &Cli, bv: ByteView<'static>, filename: String) -> Result<usize, Error> {
    let compression_level = match cli.compression_level {
        0 => 0,
        1 => 3,
        2 => 10,
        3 => 19,
        _ => 22,
    };
    let mut rv = 0;
    let archive = Archive::parse(&bv)?;
    for obj in archive.objects() {
        let obj = obj?;
        let new_filename = cli.output.join(get_target_filename(&obj.debug_id()));
        fs::create_dir_all(new_filename.parent().unwrap())?;
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
}

fn execute() -> Result<(), Error> {
    let cli = Cli::from_args();

    let mut total = 0;
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
                        total += process_file(&cli, bv, name)?;
                    }
                }

            // object file directly
            } else if Archive::peek(&bv) != FileFormat::Unknown {
                total += process_file(
                    &cli,
                    bv,
                    path.file_name().unwrap().to_string_lossy().to_string(),
                )?;
            }
        }
    }

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
