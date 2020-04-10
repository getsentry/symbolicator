# SymSorter

A small utility that takes a folder structure of debug information files
(currently mostly useful for apple and linux) and writes them into a folder
structure that symbolicator can work with as a symbol source.  The structure
used is the `unified` format that the symbolicator also supports.

## Compiling

SymSorter is not distributed precompiled so you need to compile it yourself:

```
cargo build --release
```

## Running

To run SymSorter you need to point it to a source folder (for instance `iOS
DeviceSupport`) of the SDKs you want to sort and an output folder where the
structure should be created that can be used as a symbol source.  To compress
the symbols pass `-z` for fast compression of `-zz` for good compression.
`-zzz` is also supported which however only gives diminishing returns but
slower compression and decompression performance.

In addition it needs the `prefix` that is added to the paths (this is mandatory
and typically defines the virtual bucket like `ios`, `watchos`, `macos` etc.)
and a bundle id.  The bundle ID needs to be different for each import and
ideally reflects the version of the SDK.  It needs to be unique below a
prefix.

Example:

```
./target/release/symsorter -zz -o ./output --prefix ios --bundle-id 10.3_ABCD path/to/input/folder
```

If you pass `--with-sources` it will attempt to also include source code.

## Serving

The resulting output folder should be uploaded into an S3 or GCS bucket and can then
be used as a source for system symbols.  Sentry maintains such a repository for
sentry.io but it's private due to unclear distribution rights of such symbols.
