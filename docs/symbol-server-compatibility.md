# Symbol Server Compatibility

This page describes external sources supported by Symbolicator.

The layout of external sources intends to be compatible to several symbol server implementations that have been used historically by different platforms. We commit to provide compatibility to the following services or directory structures:

- Microsoft Symbol Server
- Breakpad Symbol Repositories
- LLDB File Mapped UUID Directories
- GDB Build ID Directories

## Identifiers

There are two fundamentally different identifiers. Their semantics fundamentally depend on the symbol type, but follow certain rules:

- **Code Identifier:** Identifies the actual executable or library file (e.g. EXE or DLL)
- **Debug Identifier:** Identifies a debug companion file (e.g. PDB)

On all platforms other than Windows, binaries and debug companion files use the same file type and share the same container. The Breakpad library has thus changed the semantics of those identifiers for all other platforms to:

- **Code Identifier:** The original platform-specific identifier
- **Debug Identifier:** A potentially lossy transformation of the code identifier into a unified format similar to the PDB debug identifiers.

Specifically, the code and debug identifiers are defined as follows:

- **ELF**
- **MachO**
- **PE** / **PDB**
- **Breakpad**

## A Note on Case Sensitivity

Most symbol servers explicitly define **case insensitive** lookup semantics. This goes in particular for the Microsoft Public Symbol Server. However, the canonical representation on the file system is not necessarily case insensitive, for example when the files are stored on an Amazon S3 bucket. Since this is a hard restriction, the case for lookups is explicitly defined for each source below. Please pay attention to the casing rules!

## Breakpad

Breakpad always computes a "Breakpad ID" for each symbol. This is a lossy process depending on the file type. Sentry stores a bidirectionally compatible version of this in the `debug_id` field.

The name of the symbol file is platform dependent. On Windows, the file extension (Either *.exe*, *.dll* or *.pdb*) is replaced with *.sym*. On all other platforms, the *.sym* extension is **appended** to the full file name including potential extensions.

Casing rules are mixed:

- The file name is **as given**
- The signature part of the id (first 32 characters) are **uppercase**
- The age part of the id (remaining characters) are **lowercase**

- **Schema**: `<debug_name>/<breakpad-id>/<sym_name>`

## Microsoft Symbol Server

The public symbol server provided by Microsoft used to only host PDBs for the Windows platform. These use a *signature-age* debug identifier in addition to the file name to locate symbols. For .NET, this specification was amended by a schema for ELF and MachO-symbols, which is specified as [SSQP Key Conventions](https://github.com/dotnet/symstore/blob/master/docs/specs/SSQP_Key_Conventions.md). This means all non windows platforms are following SSQP rules except for casing.

Casing rules for Symbol Server are mixed:

- Filenames are **as given**
- Identifiers are generally **lowercase**, except:
- The signature and age of a PDB identifier is **uppercase**.
- The timestamp of a PE identifier is **uppercase** except the size is **lowercase**

- **PE**: `<code_name>/<Timestamp><SizeOfImage>/<code_name>`
- **PDB**: `<debug_name>/<Signature><Age>/<debug_name>`
- **ELF** (binary, potentially stripped): `<code_name>/elf-buildid-<note_byte_sequence>/<code_name>`
- **ELF** (debug info): `_.debug/elf-buildid-sym-<note_byte_sequence>/_.debug`
- **MachO** (binary): `<code_name>/mach-uuid-<uuid_bytes>/<code_name>`
- **MachO** (dSYM): `_.dwarf/mach-uuid-sym-<uuid_bytes>/_.dwarf`

The presence of a `index2.txt` in the root indicates two tier structure where the first two characters are prepended to the path as an additional folder. So `foo.exe/542D5742000f2000/foo.exe` is stored as `fo/foo.exe/542D5742000f2000/foo.exe`.

## Microsoft SSQP Symbol Server

Casing rules for SSQP are mixed:

- Filenames are **lowercased**
- Identifiers are generally **lowercase**, except:
- The age of a PDB identifier is **uppercase**.

- **PE**: `<code_name>/<Timestamp><SizeOfImage>/<code_name>`
- **PDB**: `<debug_name>/<Signature><Age>/<debug_name>`
- **ELF** (binary, potentially stripped): `<code_name>/elf-buildid-<note_byte_sequence>/<code_name>`
- **ELF** (debug info): `_.debug/elf-buildid-sym-<note_byte_sequence>/_.debug`
- **MachO** (binary): `<code_name>/mach-uuid-<uuid_bytes>/<code_name>`
- **MachO** (dSYM): `_.dwarf/mach-uuid-sym-<uuid_bytes>/_.dwarf`

Additionally, SSQP supports a lookup by SHA1 checksum over the file contents, commonly used for source file lookups. This will not be supported.

## LLDB Debugger (macOS)

The LLDB debugger on macOS can read debug symbols from [File Mapped UUID Directories](http://lldb.llvm.org/use/symbols.html#file-mapped-uuid-directories) (scroll down to the second last section). The UUID is broken up by splitting the first 20 hex digits into 4 character chunks, and a directory is created for each chunk. In the final directory, LLDB usually expects a symlink named by the last 12 hex digits, which it follows to the actual dSYM file. 

**Note**: this is not actually an LLVM feature. This is in fact a feature of `CoreFoundation` and exclusively implemented on macOS on top of spotlight. Spotlight indexes these paths and the private `DBGCopyFullDSYMURLForUUID` API is used by lldb to locate the symbols. macOS uses the symlinks of those locations.

Since the executable or library shares the same UUID as the dSYM file, the former are distinguished with a `.app` suffix.

The hex digits are **uppercase**, the app suffix is **lowercase**.

- **MachO** (binary): `XXXX/XXXX/XXXX/XXXX/XXXX/XXXXXXXXXXXX.app`
- **MachO** (dSYM): `XXXX/XXXX/XXXX/XXXX/XXXX/XXXXXXXXXXXX`

## GDB

GDB supports multiple lookup methods, depending on the way the debug info file is specified. However, not all of these methods are applicable to a symbol server:

- **Debug Link Method:** GDB looks up the name or relative path specified in the `.gnu.debuglink` section. This requires the debug file to be in a relative position to the actual executable, and does not provide any means to distinguish by a unique identifier.
- **Build ID Method:** Assuming that a GNU build ID note or section have been written to the ELF file, this specifies a unique identifier for the executable which is also retained in the debug file. This method is applicable to a symbol server, but only if the Build ID is present.

The GNU build ID is a variable-length binary string, usually consisting of a 20-byte SHA1 hash of the code section (`.text`). The lookup path is *nn/nnnnnnnn.debug*, where *nn* are the first 2 hex characters of the build ID bit string, and *nnnnnnnn* are the rest of the bit string. To look up executables, the `.debug` suffix is omitted.

The build-id hex representation is always provided in **lowercase**.

[https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html](https://sourceware.org/gdb/onlinedocs/gdb/Separate-Debug-Files.html)

- **ELF** (binary, potentially stripped)
- **ELF** (debug info)

## Other servers

The following additional sources were considered but are not implemented right now:

### Fedora Darkserver

In 2010, Fedora launched a project called "Darkserver" that aimed to provide a symbol server for various libraries. In 2012, it seemed to have contained symbols for Debian and Ubuntu as well.

However, this projects seems to have been abandoned since and there is only little information available. For now, there is no intention to support Darkserver.

### Mozilla Tecken

Tecken is the symbol server implementation used at Mozilla. The symbol file paths are compatible to Google's Breakpad symbol server. An example of available symbols can be viewed here: [https://symbols.mozilla.org/downloads/missing](https://symbols.mozilla.org/downloads/missing).

Tecken internally implements a client for the Microsoft Symbol Server to forward downloads for missing symbols. While doing that, it attempts to replace the last character in the URL with an underscore to look up compressed symbols. In any case Tecken tries both variants. Example: `foo.exe/<id>/foo.ex_`. Microsoft Symbol Server no longer supports this.
