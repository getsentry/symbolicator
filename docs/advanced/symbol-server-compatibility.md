# Symbol Server Compatibility

This page describes external sources supported by Symbolicator.

The layout of external sources intends to be compatible to several symbol server
implementations that have been used historically by different platforms. We
commit to provide compatibility to the following services or directory
structures:

- Microsoft Symbol Server
- Breakpad Symbol Repositories
- LLDB File Mapped UUID Directories
- GDB Build ID Directories
- debuginfod
- Unified Symbol Server Layout

## Lookup Types

Symbolicator uses different lookup types to support different servers which do
not map 1:1 to a symbol server. The two most common lookup types are `native`
and `unified`.

### `native`

This lookup type emulates the most "native" symbol server format depending
on the file type. This for instance means for PDB and PE files this turns
into Microsoft Symbol Server. It also adds support for breakpad and will
use LLDB/GDB formats for MachO and ELF respectively. This symbol server type
also has support for source bundles for all file types.

### `unified`

This is the symbolicator proprietary but preferred source (Unified Symbol
Server Layout) which adds a consistent lookup format for all architectures.
It's used at Sentry for the internal symbol lookups (like Apple or Android
symbols). Like The `native` format this supports source bundles.

## Prerequisites

### Identifiers

There are two fundamentally different identifiers. Their semantics fundamentally
depend on the symbol type, but follow certain rules:

- **Code Identifier:** Identifies the actual executable or library file (e.g.
  EXE or DLL)
- **Debug Identifier:** Identifies a debug companion file (e.g. PDB)

On all platforms other than Windows, binaries and debug companion files use the
same file type and share the same container. The Breakpad library has thus
changed the semantics of those identifiers for all other platforms to:

- **Code Identifier:** The original platform-specific identifier
- **Debug Identifier:** A potentially lossy transformation of the code
  identifier into a unified format similar to the PDB debug identifiers.

Specifically, the code and debug identifiers are defined as follows:

**ELF**:

- **Code ID:** The contents of the `.note.gnu.build-id` section, or if not
  present the value of the `NT_GNU_BUILD_ID` program header. This value is
  traditionally 20 bytes formatted as hex string (40 characters). If neither
  are present, there is no code id.
- **Debug ID:** The first 16 bytes of the GNU build id interpreted as
  little-endian GUID. This flips the byte order of the first three components
  in the build ID. An _age_ of `0` is appended at the end. The remaining bytes
  of the build ID are discarded. This identifier is only specified by
  Breakpad, but by SSQP.

**MachO**:

- **Code ID:** The UUID as specified in the `LC_UUID` load command header.
  Breakpad does not save this value explicitly since it can be converted
  bidirectionally from the UUID.
- **Debug ID:** The same UUID, amended by a `0` for age.

**WASM**:

- **Code ID:** The bytes as specified in the `build_id` custom section.
- **Debug ID:** The same as code ID but truncated to 16 bytes + `0` for age.

**PE** / **PDB**:

- **Code ID:** The hex value of the `time_date_stamp` in the COFF header
  formatted as `%08X` followed by `size_of_image` in the optional header
  formatted as `%X` (variable length). This value is always read from the PE -
  computing this from a PDB is not possible without locating the according PE.
- **Debug ID:** The `signature` and `age` (`%X` variable length) fields from
  the debug information stream in the PDB. The fields in the signature GUID
  are converted to network byte order first. This identifier can also be
  computed from a PE by reading the `code_view_pdb_70` records.

**Breakpad**:

- **Code ID:** If applicable, Breakpad symbols will contain a `INFO CODE_ID`
  record in the second line. Otherwise, the code ID is equal to the Debug ID.
- **Debug ID**: Value of the identifier field in the `MODULE` record, always
  in the first line of the file.

**Source Bundles**:

Symbolicator supports the concept of source bundles which are source archives
which can be used to extract source code for crashes. These source bundles
are an extension which are not available to all lookup types.

### Case Sensitivity

Most symbol servers explicitly define **case insensitive** lookup semantics.
This goes in particular for the Microsoft Public Symbol Server. However, the
canonical representation on the file system is not necessarily case insensitive,
for example when the files are stored on an Amazon S3 bucket. Since this is a
hard restriction, the case for lookups is explicitly defined for each source
below. Please pay attention to the casing rules!

### Compression

Symbolicator supports a range of compression formats (zlib, gzip, zstd and cab).
For cab compression, the `cabextract` binary needs to be installed. If the debug
file is already compressed, it will be auto-detected and extracted. For PE/PDB
files, Symbolicator also supports the Microsoft convention of replacing the last
character in the filename with an underscore.

## Supported Servers

### Breakpad

Breakpad always computes a "Breakpad ID" for each symbol. This is a lossy
process depending on the file type. Sentry stores a bidirectionally compatible
version of this in the `debug_id` field.

The name of the symbol file is platform dependent. On Windows, the file
extension (Either _.exe_, _.dll_ or _.pdb_) is replaced with _.sym_. On all
other platforms, the _.sym_ extension is **appended** to the full file name
including potential extensions.

Casing rules are mixed:

- The file name is **as given**
- The signature part of the id (first 32 characters) are **uppercase**
- The age part of the id (remaining characters) are **lowercase**

**Schema**: `<debug_name>/<breakpad-id>/<sym_name>`

The following layout types support this lookup:

- `native`
- `symstore`
- `symstore_index2`
- `ssqp`

### Microsoft Symbol Server

The public symbol server provided by Microsoft used to only host PDBs for the
Windows platform. These use a _signature-age_ debug identifier in addition to
the file name to locate symbols. For .NET, this specification was amended by a
schema for ELF and MachO-symbols, which is specified as [SSQP Key Conventions].
This means all non windows platforms are following SSQP rules except for casing.

Casing rules for Symbol Server are mixed:

- Filenames are **as given**
- Identifiers are generally **lowercase**, except:
- The signature and age of a PDB identifier is **uppercase**.
- The timestamp of a PE identifier is **uppercase** except the size is
  **lowercase**

**Schema**:

- **PE**: `<code_name>/<Timestamp><SizeOfImage>/<code_name>`
- **PDB**: `<debug_name>/<Signature><Age>/<debug_name>`
- **ELF** (binary, potentially stripped):
  `<code_name>/elf-buildid-<note_byte_sequence>/<code_name>`
- **ELF** (debug info): `_.debug/elf-buildid-sym-<note_byte_sequence>/_.debug`
- **MachO** (binary): `<code_name>/mach-uuid-<uuid_bytes>/<code_name>`
- **MachO** (dSYM): `_.dwarf/mach-uuid-sym-<uuid_bytes>/_.dwarf`

The presence of a `index2.txt` in the root indicates two tier structure where
the first two characters are prepended to the path as an additional folder. So
`foo.exe/542D5742000f2000/foo.exe` is stored as
`fo/foo.exe/542D5742000f2000/foo.exe`. Note that symbolicator does not probe
for the `index2.txt` file. You need to be explicit in configuring it.

Source bundles are only supported for PE/PDB files with the following format:

- **Source bundle**: `<debug_name>/<Signature><Age>/<debug_name>.src.zip`

The following layout types support this lookup:

- `symstore` for a regular symbol server
- `symstore_index2` for a symbol server with an `index2.txt` root.

### Microsoft SSQP Symbol Server

Casing rules for SSQP are mixed:

- Filenames are **lowercased**
- Identifiers are generally **lowercase**, except:
- The age of a PDB identifier is **uppercase**.

- **PE**: `<code_name>/<Timestamp><SizeOfImage>/<code_name>`
- **PDB**: `<debug_name>/<Signature><Age>/<debug_name>`
- **ELF** (binary, potentially stripped):
  `<code_name>/elf-buildid-<note_byte_sequence>/<code_name>`
- **ELF** (debug info): `_.debug/elf-buildid-sym-<note_byte_sequence>/_.debug`
- **MachO** (binary): `<code_name>/mach-uuid-<uuid_bytes>/<code_name>`
- **MachO** (dSYM): `_.dwarf/mach-uuid-sym-<uuid_bytes>/_.dwarf`

Additionally, SSQP supports a lookup by SHA1 checksum over the file contents,
commonly used for source file lookups. This is not supported.

Symbol bundles are only supported for PE/PDB files with the following format:

- **Source bundle**: `<debug_name>/<Signature><Age>/<debug_name>.src.zip`

The following layout types support this lookup:

- `ssqp`

### LLDB Debugger (macOS)

The LLDB debugger on macOS can read debug symbols from [File Mapped UUID
Directories] (scroll down to the second last section). The UUID is broken up by
splitting the first 20 hex digits into 4 character chunks, and a directory is
created for each chunk. In the final directory, LLDB usually expects a symlink
named by the last 12 hex digits, which it follows to the actual dSYM file.

**Note**: this is not actually an LLVM feature. This is in fact a feature of
`CoreFoundation` and exclusively implemented on macOS on top of spotlight.
Spotlight indexes these paths and the private `DBGCopyFullDSYMURLForUUID` API is
used by lldb to locate the symbols. macOS uses the symlinks of those locations.

Since the executable or library shares the same UUID as the dSYM file, the
former are distinguished with a `.app` suffix.

The hex digits are **uppercase**, the app suffix is **lowercase**.

- **MachO** (binary): `XXXX/XXXX/XXXX/XXXX/XXXX/XXXXXXXXXXXX.app`
- **MachO** (dSYM): `XXXX/XXXX/XXXX/XXXX/XXXX/XXXXXXXXXXXX`

Symbol bundles are supported by adding a `.src.zip` prefix to the dsym:

- **Source bundle**: `XXXX/XXXX/XXXX/XXXX/XXXX/XXXXXXXXXXXX.src.zip`

The following layout types support this lookup:

- `native`

### GDB

[GDB] supports multiple lookup methods, depending on the way the debug info file
is specified. However, not all of these methods are applicable to a symbol
server:

- **Debug Link Method:** GDB looks up the name or relative path specified in the
  `.gnu.debuglink` section. This requires the debug file to be in a relative
  position to the actual executable, and does not provide any means to
  distinguish by a unique identifier.
- **Build ID Method:** Assuming that a GNU build ID note or section have been
  written to the ELF file, this specifies a unique identifier for the executable
  which is also retained in the debug file. This method is applicable to a
  symbol server, but only if the Build ID is present.

The GNU build ID is a variable-length binary string, usually consisting of a
20-byte SHA1 hash of the code section (`.text`). The lookup path is
_nn/nnnnnnnn.debug_, where _nn_ are the first 2 hex characters of the build ID
bit string, and _nnnnnnnn_ are the rest of the bit string. To look up
executables, the `.debug` suffix is omitted.

The build-id hex representation is always provided in **lowercase**.

- **ELF** (binary, potentially stripped)
- **ELF** (debug info)
- **WASM** (debug info)

Symbol bundles are supported by adding a `.src.zip` prefix to the ELF:

- **Source bundle**: `nn/nnnnnnnn.src.zip`

The following layout types support this lookup:

- `native`

### debuginfod

Symbolicator also supports talking to
[debuginfod](https://sourceware.org/git/?p=elfutils.git;a=shortlog;h=refs/heads/debuginfod)
compatible servers for ELF and Macho.

**Schema**:

- **ELF** (binary, potentially stripped): `<code_note_byte_sequence>/executable`
- **ELF** (debug info): `<code_note_byte_sequence>/debuginfo`

Source bundles are not supported.

The following layout types support this lookup:

- `debuginfod`

### Unified Symbol Server Layout

If you have no requirements to be compatible with another system you can also
use the "unified" directory layout structure. This has the advantage that it's
unified across all platforms and thus easier to manage. It can store breakpad
files, PDBs, PEs and everything else. The `symsorter` tool in the symbolicator
repository can automatically sort debug symbols into this format and also
automatically create source bundles.

**Schema**:

The debug id is in all cases lowercase in hex format and computed as follows:

- **PE**: `<Signature><Age>` (age in hex, not padded)
- **PDB**: `<Signature><Age>` (age in hex, not padded)
- **ELF**: `<code_note_byte_sequence>`
- **MachO**: `<uuid_bytes>`
- **WASM**: `<BuildId>`

The path format is then as follows:

- binary: `<DebugIdFirstTwo>/<DebugIdRest>/executable`
- debug info: `<DebugIdFirstTwo>/<DebugIdRest>/debuginfo`
- breakpad: `<DebugIdFirstTwo>/<DebugIdRest>/breakpad`
- source bundle: `<DebugIdFirstTwo>/<DebugIdRest>/sourcebundle`

The following layout types support this lookup:

- `unified`

## Other Servers

The following additional sources were considered but are not implemented right
now:

### Fedora Darkserver

In 2010, Fedora launched a project called "Darkserver" that aimed to provide a
symbol server for various libraries. In 2012, it seemed to have contained
symbols for Debian and Ubuntu as well.

However, this projects seems to have been abandoned since and there is only
little information available. For now, there is no intention to support
Darkserver.

### Mozilla Tecken

Tecken is the symbol server implementation used at Mozilla. The symbol file
paths are compatible to Google's Breakpad symbol server. An example of available
symbols can be viewed at [symbols.mozilla.org].

Tecken internally implements a client for the Microsoft Symbol Server to forward
downloads for missing symbols. While doing that, it attempts to replace the last
character in the URL with an underscore to look up compressed symbols. In any
case Tecken tries both variants. Example: `foo.exe/<id>/foo.ex_`. Microsoft
Symbol Server no longer supports this.

[ssqp key conventions]: https://github.com/dotnet/symstore/blob/master/docs/specs/SSQP_Key_Conventions.md
[file mapped uuid directories]: http://lldb.llvm.org/use/symbols.html#file-mapped-uuid-directories
[gdb]: https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html
[symbols.mozilla.org]: https://symbols.mozilla.org/downloads/missing
