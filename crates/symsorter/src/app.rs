use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Utc};
use console::style;
// use object::macho::DyldInfoCommand;
use object::macho::{DyldCacheHeader, MachHeader32, MachHeader64};
// use object::pod::Bytes;
use object::read::macho::MachHeader;
use object::Endianness;
use rayon::prelude::*;
use serde::Serialize;
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
    #[structopt(long = "bundle-id", short = "b", value_name = "BUNDLE_ID")]
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
    bv: ByteView,
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

        if let Some(bundle_id) = &sort_config.bundle_id {
            let refs_path = new_filename.parent().unwrap().join("refs");
            fs::create_dir_all(&refs_path)?;
            fs::write(&refs_path.join(bundle_id), b"")?;
        }

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

// fn process_dyld_image(
//     sort_config: &SortConfig,
//     // data: &ByteView,
//     image: DyldCacheImage,
// ) -> Result<Vec<(String, ObjectKind)>> {
//     let mut rv = vec![];
//     let filename = image.path()?.rsplit('/').next().unwrap().to_string();

//     macro_rules! maybe_ignore_error {
//         ($expr:expr) => {
//             match $expr {
//                 Ok(value) => value,
//                 Err(err) => {
//                     if RunConfig::get().ignore_errors {
//                         eprintln!(
//                             "{}: ignored error {} ({})",
//                             style("error").red().bold(),
//                             err,
//                             style(filename).cyan(),
//                         );

//                         for cause in err.chain().skip(1) {
//                             eprintln!("{}", style(format!("  caused by {}", cause)).dim());
//                         }
//                         return Ok(rv);
//                     } else {
//                         return Err(err).context(format!("failed to process file {}", filename));
//                     }
//                 }
//             }
//         };
//     }

//     let compression_level = match sort_config.compression_level {
//         0 => 0,
//         1 => 3,
//         2 => 10,
//         3 => 19,
//         _ => 22,
//     };

//     let in_object = maybe_ignore_error!(image
//         .parse_object()
//         .map_err(|e| anyhow!("failed to parse archive {}", e)));

//     let writable_data = clone_dyld_image(&in_object);

//     let raw_kind = if in_object.is_64() {
//         MachHeader64::parse(writable_data.as_slice(), 0)?.filetype(in_object.endianness())
//     } else {
//         MachHeader32::parse(writable_data.as_slice(), 0)?.filetype(in_object.endianness())
//     };
//     let kind = match raw_kind {
//         object::macho::MH_OBJECT => ObjectKind::Relocatable,
//         object::macho::MH_EXECUTE => ObjectKind::Executable,
//         object::macho::MH_FVMLIB => ObjectKind::Library,
//         object::macho::MH_CORE => ObjectKind::Dump,
//         object::macho::MH_PRELOAD => ObjectKind::Executable,
//         object::macho::MH_DYLIB => ObjectKind::Library,
//         object::macho::MH_DYLINKER => ObjectKind::Executable,
//         object::macho::MH_BUNDLE => ObjectKind::Library,
//         object::macho::MH_DSYM => ObjectKind::Debug,
//         object::macho::MH_KEXT_BUNDLE => ObjectKind::Library,
//         _ => ObjectKind::Other,
//     };

//     let root = &RunConfig::get().output;
//     let new_filename = root.join(maybe_ignore_error!(get_dyld_image_filename(
//         &in_object, &kind
//     )
//     .ok_or_else(|| anyhow!("unsupported file"))));

//     fs::create_dir_all(new_filename.parent().unwrap())?;

//     if let Some(bundle_id) = &sort_config.bundle_id {
//         let refs_path = new_filename.parent().unwrap().join("refs");
//         fs::create_dir_all(&refs_path)?;
//         fs::write(&refs_path.join(bundle_id), b"")?;
//     }

//     // all of the types recognized by DyldCacheHeader
//     let arch = match in_object.architecture() {
//         object::Architecture::I386 => Arch::X86,
//         object::Architecture::PowerPc => Arch::Ppc,
//         // we lose a bit of precision in the below variants because object combines all x86_64, arm,
//         // and arm64 variants into one common enum member
//         object::Architecture::X86_64 => Arch::Amd64,
//         object::Architecture::Arm => Arch::Arm,
//         object::Architecture::Aarch64 => Arch::Arm64_32,
//         _ => Arch::Unknown,
//     };

//     let meta = DebugIdMeta {
//         name: Some(filename.clone()),
//         arch: Some(arch),
//         file_format: Some(FileFormat::MachO),
//     };

//     fs::write(
//         &new_filename.parent().unwrap().join("meta"),
//         &serde_json::to_vec(&meta)?,
//     )?;

//     log!(
//         "{} ({}, {}) -> {}",
//         style(&filename).dim(),
//         style(kind).yellow(),
//         style(arch).yellow(),
//         style(new_filename.display()).cyan(),
//     );
//     let mut out = fs::File::create(&new_filename)?;

//     if compression_level > 0 {
//         copy_encode(writable_data.as_slice(), &mut out, compression_level)?;
//     } else {
//         io::copy(&mut writable_data.as_slice(), &mut out)?;
//     }
//     rv.push((get_dyld_image_unified_id(&in_object), kind));

//     Ok(rv)
// }

fn sort_files(sort_config: &SortConfig, paths: Vec<PathBuf>) -> Result<(usize, usize)> {
    let mut source_bundles_created = 0;
    let source_candidates = Mutex::new(HashMap::<String, Option<PathBuf>>::new());
    let debug_ids = Mutex::new(Vec::new());

    log!("{}", style("Sorting debug information files").bold());

    paths
        .into_iter()
        .map(WalkDir::new)
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.metadata().ok().map_or(false, |x| x.is_file()))
        .par_bridge()
        .map(|entry| {
            let path = entry.path();
            let bv = match ByteView::open(path) {
                Ok(bv) => bv,
                Err(_) => return Ok(()),
            };

            // zip archive
            if bv.get(..2) == Some(b"PK") {
                let mut zip = ZipArchive::new(io::Cursor::new(&bv[..]))?;
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
            // dyld shared cache
            // } else if let Ok(cache) = DyldCache::<Endianness>::parse(bv.as_slice()) {
            } else if let Ok(header) = DyldCacheHeader::<Endianness>::parse(bv.as_slice()) {
                let data = bv.as_slice();
                let (_, endian) = header.parse_magic()?;
                let mappings = header.mappings(endian, data)?;
                let images = header.images(endian, data)?;

                for image in images.iter().take(2) {
                    let mut image_data = Vec::<u8>::new();
                    let image_path = match image.path(endian, data).map(|p| std::str::from_utf8(p))
                    {
                        Ok(Ok(path)) => path,
                        Ok(Err(_)) | Err(_) => {
                            log!("unable to grab pathname for image");
                            continue;
                        }
                    };
                    let image_offset = match image.file_offset(endian, mappings) {
                        Ok(offset) => offset,
                        Err(_) => {
                            log!("unable to obtain offset for {:?}", image_path);
                            continue;
                        }
                    };
                    let kind = match object::FileKind::parse_at(data, image_offset) {
                        Ok(file) => file,
                        Err(err) => {
                            log!("unable to parse file {:?}: {}", image_path, err);
                            continue;
                        }
                    };
                    log!("image: {}", image_path);
                    match kind {
                        object::FileKind::MachO32 => {
                            let header = MachHeader32::<Endianness>::parse(data, image_offset)?;
                            // 28 bytes for 32bit
                            let start_header = image_offset as usize;
                            let end_header =
                                start_header + std::mem::size_of::<MachHeader32<Endianness>>();
                            // TODO: remove in-cache bit from flags, flags &= 0x7FFFFFFF
                            image_data.extend_from_slice(&data[start_header..end_header]);

                            // #SEGMENT getting size based on values in header
                            let start_commands = end_header;
                            let end_commands = end_header + (header.sizeofcmds(endian) as usize);
                            image_data.extend_from_slice(&data[start_commands..end_commands]);

                            // header.load_commands(endian, data, image_offset)
                        }
                        object::FileKind::MachO64 => {
                            let header = MachHeader64::<Endianness>::parse(data, image_offset)?;
                            // 32 bytes for 64bit
                            let start_header = image_offset as usize;
                            let end_header =
                                start_header + std::mem::size_of::<MachHeader64<Endianness>>();
                            image_data.extend_from_slice(&data[start_header..end_header]);

                            // #SEGMENT getting size based on values in header
                            let start_commands = end_header;
                            let end_commands = end_header + (header.sizeofcmds(endian) as usize);
                            image_data.extend_from_slice(&data[start_commands..end_commands]);

                            // header.load_commands(endian, data, image_offset)

                            let mut commands =
                                match header.load_commands(endian, data, image_offset) {
                                    Ok(cmds) => cmds,
                                    Err(_) => {
                                        log!("could not load commands for {}", image_path);
                                        continue;
                                    }
                                };

                            let mut index = 0;
                            while let Ok(Some(command)) = commands.next() {
                                log!("cmdsize: {}", command.cmdsize());
                                if let Ok(variant) = command.variant() {
                                    match variant {
                                        object::read::macho::LoadCommandVariant::Segment32(
                                            _,
                                            _,
                                        ) => log!("ok"),
                                        object::read::macho::LoadCommandVariant::Segment64(
                                            _,
                                            _,
                                        ) => log!("ok"),
                                        object::read::macho::LoadCommandVariant::DyldInfo(
                                            dyld_info,
                                        ) => {
                                            log!("index: {}", index);
                                            log!("dyldinfo: {:?}", dyld_info);
                                            log!("cmd rep: {:?}", command);

                                            // whoops, sizeofcmds is the size for ALL cmds, not per cmd
                                            // let start = start_commands
                                            //     + (index * header.sizeofcmds(endian)) as usize;
                                            // let end = start + header.sizeofcmds(endian) as usize;

                                            // let mut podbytes = Bytes(&data[start..end]);

                                            // let wow: &DyldInfoCommand<Endianness> =
                                            //     match podbytes.read() {
                                            //         Ok(command) => command,
                                            //         Err(_) => {
                                            //             log!("oh no");
                                            //             index += 1;
                                            //             continue;
                                            //         }
                                            //     };
                                        }
                                        // specifically LC_DYLD_EXPORTS_TRI, LC_FUNCTION_STARTS, LC_DATA_IN_CODE
                                        // REMOVE LC_SEGMENT_SPLIT_INFO
                                        object::read::macho::LoadCommandVariant::LinkeditData(
                                            _,
                                        ) => {
                                            log!("ok");
                                        }
                                        object::read::macho::LoadCommandVariant::Symtab(_) => {
                                            log!("ok");
                                        }
                                        object::read::macho::LoadCommandVariant::Dysymtab(_) => {
                                            log!("ok");
                                        }
                                        // LC_LOAD_DYLIB, etc
                                        object::read::macho::LoadCommandVariant::Dylib(_) => {
                                            log!("ok");
                                        }
                                        _ => {
                                            // on second thought, just copy everything else over that doesn't
                                            // need adjusting but maybe emit something if it isn't expected?
                                            log!("unexpected load command: {:?}", command);
                                            index += 1;
                                            continue;
                                        }
                                    }
                                }
                                index += 1;
                            }
                        }
                        other => {
                            log!(
                                "found unexpected file type in dyld shared cache: {:?}, {}",
                                other,
                                image_path
                            );
                            continue;
                        }
                    };

                    let root = &RunConfig::get().output;
                    let filename = root.join(image_path.rsplit('/').next().unwrap().to_string());
                    fs::create_dir_all(filename.parent().unwrap())?;
                    let mut out = fs::File::create(&filename)?;
                    io::copy(&mut image_data.as_slice(), &mut out)?;
                }

                // for image in cache.images() {
                //     println!("path: {}", image.path().unwrap_or_default());

                //     debug_ids.lock().unwrap().extend(
                //         process_dyld_image(sort_config, image)?
                //             .into_iter()
                //             .map(|x| x.0),
                //     );
                // }
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

    let debug_ids = debug_ids.into_inner().unwrap();
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
            "No compression used. Consider passing in -zz."
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
