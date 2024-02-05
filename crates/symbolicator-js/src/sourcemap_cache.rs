use std::fs::File;
use std::io::{self, BufWriter};
use std::sync::Arc;

use futures::future::BoxFuture;
use symbolic::common::{ByteView, SelfCell};
use symbolic::debuginfo::sourcebundle::SourceFileDescriptor;
use symbolic::sourcemapcache::{SourceMapCache, SourceMapCacheWriter};
use symbolicator_service::caches::versions::SOURCEMAP_CACHE_VERSIONS;
use symbolicator_service::caches::ByteViewString;
use symbolicator_service::caching::{CacheEntry, CacheError, CacheItemRequest, CacheVersions};
use symbolicator_service::objects::ObjectHandle;
use tempfile::NamedTempFile;

use crate::lookup::{
    open_bundle, ArtifactBundle, ArtifactBundles, CachedFile, CachedFileUri, FileKey,
    OwnedSourceMapCache,
};
use crate::utils::get_release_file_candidate_urls;

#[derive(Clone, Debug)]
pub enum SourceMapContents {
    Lazy(Arc<ObjectHandle>, FileKey),
    Eager(ByteViewString),
}

impl SourceMapContents {
    pub fn from_cachedfile(
        artifact_bundles: &ArtifactBundles,
        sourcemap_uri: &CachedFileUri,
        sourcemap: CachedFile,
    ) -> Self {
        if !sourcemap.is_lazy {
            return Self::Eager(sourcemap.contents);
        }

        let bundle = if let CachedFileUri::Bundled(bundle_uri, key) = sourcemap_uri {
            artifact_bundles.get(bundle_uri).and_then(|bundle| {
                let Ok((bundle, _)) = bundle else { return None };
                let bundle = bundle.owner().clone();
                let contents = Self::Lazy(bundle, key.clone());
                Some(contents)
            })
        } else {
            None
        };

        bundle.unwrap_or(Self::Eager(sourcemap.contents))
    }
}

#[derive(Clone, Debug)]
pub struct FetchSourceMapCacheInternal {
    pub source: ByteViewString,
    pub sourcemap: SourceMapContents,
}

impl CacheItemRequest for FetchSourceMapCacheInternal {
    type Item = OwnedSourceMapCache;

    const VERSIONS: CacheVersions = SOURCEMAP_CACHE_VERSIONS;

    fn compute<'a>(&'a self, temp_file: &'a mut NamedTempFile) -> BoxFuture<'a, CacheEntry> {
        Box::pin(async move {
            let sourcemap = match &self.sourcemap {
                SourceMapContents::Lazy(bundle, key) => {
                    let bundle = open_bundle(bundle.clone())?;
                    let descriptor = get_descriptor_from_bundle(&bundle, key);

                    let contents = descriptor
                        .and_then(|d| d.into_contents())
                        .ok_or_else(|| {
                            CacheError::Malformed("descriptor should have `contents`".into())
                        })?
                        .into_owned();
                    ByteViewString::from(contents)
                }
                SourceMapContents::Eager(contents) => contents.clone(),
            };

            write_sourcemap_cache(temp_file.as_file_mut(), &self.source, &sourcemap)
        })
    }

    fn load(&self, data: ByteView<'static>) -> CacheEntry<Self::Item> {
        parse_sourcemap_cache_owned(data)
    }
}

fn parse_sourcemap_cache_owned(byteview: ByteView<'static>) -> CacheEntry<OwnedSourceMapCache> {
    SelfCell::try_new(byteview, |p| unsafe {
        SourceMapCache::parse(&*p).map_err(CacheError::from_std_error)
    })
}

/// Computes and writes the SourceMapCache.
#[tracing::instrument(skip_all)]
fn write_sourcemap_cache(file: &mut File, source: &str, sourcemap: &str) -> CacheEntry {
    tracing::debug!("Converting SourceMap cache");

    let smcache_writer = SourceMapCacheWriter::new(source, sourcemap)
        .map_err(|err| CacheError::Malformed(err.to_string()))?;

    let mut writer = BufWriter::new(file);
    smcache_writer.serialize(&mut writer)?;
    let file = writer.into_inner().map_err(io::Error::from)?;
    file.sync_all()?;

    Ok(())
}

fn get_descriptor_from_bundle<'b>(
    bundle: &'b ArtifactBundle,
    key: &FileKey,
) -> Option<SourceFileDescriptor<'b>> {
    let bundle = bundle.get();
    let ty = key.as_type();

    if let Some(debug_id) = key.debug_id() {
        if let Ok(Some(descriptor)) = bundle.source_by_debug_id(debug_id, ty) {
            return Some(descriptor);
        }
    }
    if let Some(abs_path) = key.abs_path() {
        for url in get_release_file_candidate_urls(abs_path) {
            if let Ok(Some(descriptor)) = bundle.source_by_url(&url) {
                return Some(descriptor);
            }
        }
    }
    None
}
