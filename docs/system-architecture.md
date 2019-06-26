# System Architecture

This page describes the internal structure and processes of Symbolicator.

## Looking up debug/unwind information

When finding an object file for a image in a symbolication request, we do the following:

1. We generate all possible file downloads for each source, based on platform
  information and source filters. See [Symbol Lookup](symbol-lookup.md).

2. We download all files we can get and cache them keyed by source ID and
   filepath (`ObjectFile`).

3. Then we rank all downloaded files:

  1. Objects with debug or unwind information (depending on what we want to use the object for) are ranked at the top.
  2. Objects with a symbol table are ranked second.
  3. Unparseable objects are ranked somewhere at the bottom together with empty files, 404s etc.

3. Then we generate a symcache or cficache from the best file, save it also keyed by source ID and filepath. In the cache dir, a symcache file now basically has the same filename as an object file, just in a different folder.

### `ObjectMeta`

The issue with the above setup is that one would have to have all objects permanently persisted on disk to be able to decide which symcache to use, which is only feasible in a world with infinite disk space.

For this purpose we have the concept of an `ObjectMeta` cache, which stores information about the quality of an `ObjectFile` and is persisted for much longer. `ObjectFile`s are deleted just a few days after download while `ObjectMeta`s stick around for as long as the symcache/cficache does.

### The object cache key

As mentioned earlier, `ObjectFile`s, symcaches/cficaches (and `ObjectMeta`s) are cached by source ID and filepath. What this actually means depends on the source type:

* For the HTTP source type (Microsoft SymStore etc) this is just the source ID
  as given in the symbolication request + the filepath of the download URL.
* For S3 and GCS buckets this is the source ID and the object key/path (object
  as in S3 object).
* For the Sentry source type this is the source ID and the numeric "Debug File
  Id".

In summary, the cache key is just whatever is necessary to uniquely identify a file
within a source.


### Generating all possible file downloads for a source

In step 1, we mentioned that we generate all possible file downloads. For most filesystem-like source types this is a simple computation that takes an `ObjectId` (really a quintuple of debug and code ID + debug name + code name) and generates possible filepaths. The logic for this lives in `src/utils/paths.rs`.

For the Sentry source type, this looks a bit different. Since Sentry already builds some sort of index based on debug and code ID, we first have to issue an HTTP request to Sentry to search for files based on those IDs. We get back a list of numeric "Debug File Ids" (those are just the integer PK in Postgres) that we then can download one by one.

Since this step is quite expensive for Sentry, it is cached separately. This part is currently WIP.

### Example

The logic for looking up debug information for an image looks as follows (simplified, conflating code_name vs debug_name + code_id vs debug_id):

* Lookup of object `name=wkernel32.pdb, id=0xdeadbeef` (uncached, parallelized):
  * (cached) Fetching sentry index for `id=0xdeadbeef` (uncached) -> returns `[123456]`
  * (cached) lookup of object *metadata* `[sentry, 123456]` -> returns `has_debug_info=true, ...`
    * (cached) lookup of object *file* `[sentry, 123456]`
  * (cached) lookup of object *metadata* `[microsoft, /wkernel32.pdb/deadbeef/wkernel32.pdb]` -> returns `has_debug_info=false, ...`
    * (cached) lookup of object *file* `[microsoft, /wkernel32.pdb/deadbeef/wkernel32.pdb]`
  * Sentry project symbol wins, because it has more than symbol table
* (cached) lookup of symcache `[sentry, 123456]`

All steps that are behind a caching layer are prefixed with `(cached)`. If a cache hit occurs, the sub-bulletpoints are not executed.
