# Caching

Conceptionally, Symbolicator does not store debug information files but treats
them as transient caches. Symbolicator caches two types of files: Raw debug
information files and symbolication caches.

## Cache Files

Symbolicator caches two kinds of files: original Debug Information Files and
derived caches which comprise SymCaches and CFI caches.

- **Debug Information Files (Object Files)** (original): The original debug
  files as they are stored on external sources. They are cached to allow for
  faster recomputation of the derived caches, especially when after an update
  the format of the derived caches change. Otherwise, all original files would
  have to be downloaded again which could incur significant performance impact.
  DIFs are platform dependent and usually large in size, up to multiple GB.
  Their internal structure is platform dependent.
- **Object Meta Files** (derived): Important attributes of a Object File used to
  determine which Object File is the best one. This information is persisted
  separately from Object Files because we want to delete Object Files a few days
  after download for disk we want to delete Object Files a few days after
  download for disk space while still knowing some information about them.
- **Symbolication Caches** (derived): A platform-independent representation of
  function and line information to symbolicate instruction addresses. Those
  files are usually significantly smaller than native DIFs. However, endianness
  of the file contents depends on the host so these files cannot be transferred
  between systems with different endianness.
- **CFI Caches** (derived): A platform-independent representation of stack
  unwind information to allow stackwalking. This currently uses the Breakpad
  ASCII format.

## Cache Rules

In addition to caching DIFs and derived caches, Symbolicator also stores
placeholders to indicate the absence of or errors when retrieving or computing
those files. The cache adheres to the following rules:

1. DIFs are cached until they have not been used for _7 days_ but may be deleted
   earlier when disk space is running out. A DIF is used when a derived cache is
   computed from it.
2. Derived caches that were successfully created from preferred DIFs are cached
   until they have not been used for _at least 7 days_.
3. Derived caches that were successfully created from fallback DIFs are treated
   equally but _at most once per hour_ an upgrade attempt is started to search
   for better DIFs.
4. The absence of a DIF is cached for _1 hour_, after that another fetch attempt
   from all sources is started.
5. Failed conversions (due to malformed or unsupported debug files) are cached
   for _24 hours_ but only up to the _next restart_. After that, another
   conversion is attempted. The restart constraint serves the purpose to allow
   immediate bug fixes.

Derived caches can continue to be stored independently of the DIFs they were
created from. Because they are smaller than the originals, this contributes to a
better use of the available disk space. All timeouts mentioned above are
configurable:

```yml
caches:
  downloaded:
    max_unused_for: 7d # unused DIFs, rule 1
    retry_misses_after: 1h # absence of a DIF, rule 4
  derived:
    max_unused_for: 7d # unused caches, rule 2
    retry_misses_after: 1h # also necessary for rule 4
```

## Cache Enforcement

In order to enforce the desired cache behavior, Symbolicator uses file system
meta data and information stored in the files themselves to determine when to
evict items from the cache.

1. **Last use of derived cache**: The time when a derived cache was last used in
   a symbolication request. This allows to prune cold caches from the system,
   even from an external service. This value is stored in the _mtime_ of the
   symbolication file and updated at most _once per hour_.
2. **Last attempt to fetch dependencies of a cache:** The time when the a
   derived cache could not be constructed because no debug files were available.
   Instead, a placeholder cache file is created and its _mtime_ indicates the
   attempt. If this file is older than the threshold (default _1 hour_), another
   fetch attempt can be started and the file is recreated or replaced. **NOTE**:
   The placeholder is created for the derived cache only, because otherwise too
   many placeholders would be created.
3. **Last failed cache conversion**: The time when deriving a cache from a
   native DIF failed due to an error. A placeholder file is created and its
   _mtime_ indicates the attempt. If a file is older than the threshold (default
   _24 hours_), conversion is repeated. If the debug files have been deleted in
   between, this causes another fetch from external sources.
4. **DIFs used for derived cache**: The kind of debug information that was used
   to compute a derived cache. SymCaches store this in their header, so this can
   be retrieved inexpensively by opening the file. CFI caches currently do not
   contain this information, although all DIFs offer equal quality of unwind
   information.
5. **Last cache upgrade:** The last time when an upgrade of an existing derived
   cache was attempted since it was not computed from a preferred source. This
   also uses the file’s _mtime_ and attempts an update every time the
   modification time exceeds the threshold.

## Scopes

Cached files are associated to a scope, which is given by the symbolication
request. The default is a “global” scope, which implicitly makes the files
accessible to any request (scoped and not scoped). Otherwise, the scope needs to
match for caches to be used. Note that there is no verification of the scope
number, so users of Symbolicator are required to ensure the soundness of scope
values.

Scoping is achieved by encoding the scope identifier into the cache paths, thus
creating separate cache directories for each scope.

## Pruning Caches

The `symbolicator cleanup` command removes stale caches. This command needs to
be run manually and periodically, or at least when disk space is about to run
out.

Symbolicator operates under the assumption that files may be removed by an
external actor at any time (one such actor is `symbolicator cleanup` itself
which does not really attempt to synchronize with the main symbolicator
service).

Symbolicator assumes a fully POSIX-compliant filesystem to be able to serve
requests without interruptions while files are being deleted. **Using a network
share for the cache folder will not work.**
