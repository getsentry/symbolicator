use std::io::Cursor;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use lazy_static::lazy_static;
use regex::Regex;
use symbolic::common::ByteView;
use symbolic::debuginfo::sourcebundle::SourceBundleWriter;
use symbolic::debuginfo::{Archive, FileFormat, Object, ObjectKind};

lazy_static! {
    static ref BAD_CHARS_RE: Regex = Regex::new(r"[^a-zA-Z0-9.,-]+").unwrap();
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
        ObjectKind::Relocatable | ObjectKind::Library | ObjectKind::Executable => "executable",
        _ => bail!("unsupported file"),
    };
    Ok(format!("{}/{}/{}", &id[..2], &id[2..], suffix).into())
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
            if writer.write_object(&obj, &name).map_err(|e| anyhow!(e))? {
                return Ok(Some(ByteView::from_vec(out)));
            }
        }
    }
    Ok(None)
}

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
