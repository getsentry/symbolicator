# Source Bundles

Symbolicator supports the concept of source bundles.  These are zip archives
containing source code that goes along debug information files.  These archives
can be created with the `sentry-cli` tool as well as `symsorter` and are also
supported by the underlying `symbolic` library.

Source bundles are ZIP archives with a well-defined internal structure. Most
importantly, they contain source files in a nested directory structure.
Additionally, there is meta data associated to every source file, which
allows to store additional properties, such as the original file system path,
a web URL, and custom headers.

## Header

The source bundle is a regular ZIP archive but has a special header prepended
to identify it uniquely as source bundle.

The header is in little endian:

```
BUNDLE_MAGIC:    [u8; 4]
BUNDLE_VERSION:  u32
```

The bundle magic is always ASCII "SYSB" (symbolic source bundle)

## Structure

The internal structure is as follows:

```txt
manifest.json
files/
  file1.txt
  subfolder/
    file2.txt
```

## Manifest

The manifest is a JSON file with the following structure:

```json
{
  "files": {
    "path/in/bundle": {
      "type": "source",
      "path": "original/file/name",
    }
  },
  "code_id": "CODE_ID",
  "debug_id": "DEBUG_ID",
  "object_name": "OBJECT_NAME",
  "arch": "ARCHITECTURE"
}
```

The attributes must match the attributes of the debug information file (.pdb,
or DWARF file) that goes along with it.

Example Manifest:

```json
{
  "files": {
    "files/actix-web-0.7.19/src/server/h1.rs": {
      "type":"source",
      "path":
      "/usr/local/cargo/registry/src/github.com-1ecc6299db9ec823/actix-web-0.7.19/src/server/h1.rs"
    },
    ...
  },
  "arch": "x86_64",
  "code_id": "f53af9062b83bc3b6dfd27a46912605f23e874de",
  "debug_id": "06f93af5-832b-3bbc-6dfd-27a46912605f",
  "object_name": "relay"
}
```

In this case the source file needs to be stored in the following location
in the bundle: `files/actix-web-0.7.19/src/server/h1.rs`.
