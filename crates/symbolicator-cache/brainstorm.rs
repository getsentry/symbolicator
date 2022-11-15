// timer / debouncing based eviction
// when to retry


// layer 1:
// - sync access to slots (by key)
// layer 2:
// - check if recomputation is needed -> checks dependent caches / files
// layer 3:
// - computation


struct Cache {
    map: HashMap<key, CacheEntryComputation>
}

ComputationFactory -> impl Future<CacheEntry>

impl Cache {
    pub async fn gimme(key) -> CacheEntry {
        let slot = map.slot;

        if !slot {
            spawn computation -> shared future
            slot = slot.insert shared future
        }

        loop {
            let entry = slot.await

            if entry.valid {
                return entry;
            }

            spawn computation -> shared future
            slot = slot.insert shared future
        }
    }
}

type CacheEntryComputation = futures::future::Shared<impl Future<Output = CacheEntry>>;

struct CacheEntrySlot {
    computation: CacheEntryComputation,
    eviction_deadline: Instant, // creation time + X
}

enum CacheEntry { // Arc-ed
    valid_until: Instant, // based on disk timestamp & refresh_after config
    candidates: Vec<...>,

    Ok(...), // ByteView ... parsed Cache File
      // -> version
    NotFound(Error), // Download / Permission Error
    Malformed(Error), // Conversion / Parsing Error
}

// moka =>
struct InternalCacheEntry<T> {
    entry: CacheEntry<T>,
    ttl: Instant,
}

enum CacheEntry<T> {
    AllGood(T), // => symcache => SymCache, object_meta => ObjectFeatures, cficache => SymbolFile
    NotFound,
    PermissionDenied(String), // => whatever the server returned
    Timeout(Duration), // => should we return the duration after which we timed out?
    DownloadError(String), // => connection lost, dns resolution, or 5xx server errors
    Malformed(String), // => unsupported object file, symcache conversion failure
    InternalError, // => unexpected errors like: io::Error, serde_error, symcache parse error,
}

impl<T> CacheEntry<T> {
    fn load_from_file() -> Self<ByteView<'static>>;

    fn persist_to_file(&self); // => T would not be directly
}

fetch_symcache(sources, id) -> CacheEntry<SymCache> {
    // candidates blabla
    // object_meta

    moka:
        load_disk_cache(disk_cache_key) -> CacheEntry<ByteView<'static>>

        load_symcache(CacheEntry<ByteView>) -> Arc<CacheEntry<SymCache>>
}
