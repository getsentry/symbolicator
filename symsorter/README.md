# SymSorter

A small utility that takes a folder structure of debug information files
(currently really only useful for iOS) and writes them into a folder structure
that symbolicator can work with as a symbol source.

This is used for take iOS system symbols and to bring them into a common
format.

## Compiling

SymSorter is not distributed precompiled so you need to compile it yourself:

```
cargo build --release
```

## Running

To run SymSorter you need to point it to a source folder (`iOS DeviceSupport`) of the
SDKs yo want to sort and an output folder where the structure should be created that
can be used as a symbol source.  SymSorter will do weird things if you point it to
other debug symbols.  To compress the symbols pass `-z` for fast compression of `-zz`
for good compression.  `-zzz` is also supported which however only gives diminishing
returns but slower compression and decompression performance.

Example:

```
./target/release/symsorter -zz -o ./output path/to/input/folder
```

## Serving

The resulting output folder should be uploaded into an S3 or GCS bucket and can then
be used as a source for iOS symbols.  Sentry maintains such a repository for sentry.io
but it's private due to unclear distribution rights of such symbols.
