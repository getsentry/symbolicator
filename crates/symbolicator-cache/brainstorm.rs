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
