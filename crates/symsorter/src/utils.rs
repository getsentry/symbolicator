use crate::config::RunConfig;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{Result, anyhow, bail};
use regex::Regex;
use symbolic::common::ByteView;
use symbolic::debuginfo::sourcebundle::{SourceBundleWriter, SourceFileDescriptor};
use symbolic::debuginfo::{Archive, FileEntry, FileFormat, Object, ObjectKind};

static BAD_CHARS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^a-zA-Z0-9.,-]+").unwrap());

/// Console logging for the symsorter app.
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        {
            if (!RunConfig::get().quiet) {
                println!($($arg)*);
            }
        }
    }
}

/// Makes a safe bundle ID from arbitrary input.
pub fn make_bundle_id(input: &str) -> String {
    BAD_CHARS_RE
        .replace_all(input, "_")
        .trim_matches(&['_'][..])
        .to_string()
}

/// Checks if something is a safe bundle id
pub fn is_bundle_id(input: &str) -> bool {
    make_bundle_id(input) == input
}

/// Gets the unified ID from an object.
pub fn get_unified_id(obj: &Object) -> Result<String> {
    if obj.file_format() == FileFormat::Pe || obj.code_id().is_none() {
        let debug_id = obj.debug_id();
        if debug_id.is_nil() {
            Err(anyhow!("failed to generate debug identifier"))
        } else {
            Ok(debug_id.breakpad().to_string().to_lowercase())
        }
    } else {
        Ok(obj.code_id().as_ref().unwrap().as_str().to_string())
    }
}

/// Returns the intended target filename for an object.
pub fn get_target_filename(obj: &Object) -> Result<PathBuf> {
    let id = get_unified_id(obj)?;
    // match the unified format here.
    let suffix = match obj.kind() {
        ObjectKind::Debug => "debuginfo",
        ObjectKind::Sources if obj.file_format() == FileFormat::SourceBundle => "sourcebundle",
        ObjectKind::Library | ObjectKind::Executable => "executable",
        _ => bail!("unsupported file"),
    };
    Ok(format!("{}/{}/{}", &id[..2], &id[2..], suffix).into())
}

// Filters very large files, embedded source files, and Precompiled headers from Source bundles.
pub fn filter_bad_sources(
    entry: &FileEntry,
    embedded_source: &Option<SourceFileDescriptor>,
) -> bool {
    let max_size = 10 * 1024 * 1024; // Files over 10MB.
    let path = &entry.abs_path_str();

    // Ignore pch files.
    if path.ends_with(".pch") {
        log!("Skipping precompiled header: {}", path);
        return false;
    }

    // Ignore files embedded in the object itself.
    if embedded_source.is_some() {
        log!("Skipping embedded source file: {}", path);
        return false;
    }

    // Ignore files larger than limit, currently 10MB.
    if let Ok(meta) = fs::metadata(path) {
        let item_size = meta.len();
        if meta.len() > max_size {
            log!(
                "Source exceeded maximum item size limit ({}). {}",
                item_size,
                path
            );
            return false;
        }
    }
    true
}

/// Creates a source bundle from a path.
pub fn create_source_bundle(path: &Path, unified_id: &str) -> Result<Option<ByteView<'static>>> {
    let bv = ByteView::open(path)?;
    let archive = Archive::parse(&bv).map_err(|e| anyhow!(e))?;
    for obj in archive.objects() {
        let obj = obj.map_err(|e| anyhow!(e))?;
        if matches!(get_unified_id(&obj), Ok(generated_id) if unified_id == generated_id) {
            let mut out = Vec::<u8>::new();
            let writer =
                SourceBundleWriter::start(Cursor::new(&mut out)).map_err(|e| anyhow!(e))?;
            let name = path.file_name().unwrap().to_string_lossy();
            if writer
                .write_object_with_filter(&obj, &name, filter_bad_sources)
                .map_err(|e| anyhow!(e))?
            {
                return Ok(Some(ByteView::from_vec(out)));
            }
        }
    }
    Ok(None)
}
