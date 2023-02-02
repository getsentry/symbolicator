# Lookup Strategy

Symbolicator queries a list of external sources for debug information files
which it uses to process minidumps and symbolicate stack traces. Based on
availability, the CPU architecture and the operating system, it may determine
the "best" file to use:

1. Compute the code and debug identifiers for the modules to load.
2. Look up available files in all sources
3. Choose the best available file

## Identifiers

Symbol servers use native identifiers to locate files. Usually, they put them in
a directory hierarchy for easier retrieval.

There are two kinds of identifiers:

- **Code Id:** Native identifier of the actual binary (library or executable).
  Usually, this is a value stored in the header or computed from its contents.
- **Debug Id:** Identifier of the associated debug file on some platforms, or a
  mangled version of the code identifier on other platforms.

To look up different symbol kinds, the symbol server uses the following process
to determine the ID (see below for exact conversion algorithms):

- **MachO:** Use the UUID which is stored interchangeably in `debug_id` and
  `code_id`. If one of these values is missing, it can be computed from the
  other. Primarily, the `code_id` should be used.
- **ELF:** Use the GNU build id which is given in the `code_id`. The `debug_id`
  can be computed from this `code_id`, but not the other way around. Thus, if
  the `code_id` is missing, lookups may not be possible.
- **PE**: Windows executables use a combination of timestamp and size fields to
  compute a `code_id`. It is mandatory, as it cannot be derived from the
  `debug_id`.
- **PDB**: Windows PDBs have their own `debug_id`, which can be read from the
  PDB or PE header. It is mandatory, as it cannot be derived from the `code_id`.
- **Portable PDB**: Like PDBs, portable PDBs have their own `debug_id`.
- **Breakpad**: Google Breakpad uses `debug_id` for all symbol kinds. If it is
  missing, it can be computed from the `code_id` if this is possible using the
  above rules.

## Symbol Precedence

Depending on the desired information and type of image, the Symbolicator
requests several files and chooses the best one. Note that the image type
depends on the platform, but there are multiple formats for debug information
files available.

The following is a table of what we initially deemed as ideal lookup strategy.
How we deviate from this in practice is documented separately.

### Symbol Table

| Platform | 1. Choice    | 2. Choice    | 3. Choice |
| -------- | ------------ | ------------ | --------- |
| MachO    | MachO (dSYM) | MachO (code) | Breakpad  |
| ELF      | ELF (debug)  | ELF (code)   | Breakpad  |
| PE       | PDB          | PE           | Breakpad  |

### Debug Information

| Platform | 1. Choice    | 2. Choice  | 3. Choice |
| -------- | ------------ | ---------- | --------- |
| MachO    | MachO (dSYM) | Breakpad   |           |
| ELF      | ELF (debug)  | ELF (code) | Breakpad  |
| PE       | PDB          | Breakpad   |           |
| .NET PE  | Portable PDB |            |           |

### Unwind Information

| Platform    | 1. Choice    | 2. Choice |
| ----------- | ------------ | --------- |
| MachO       | MachO (code) | Breakpad  |
| ELF         | ELF (code)   | Breakpad  |
| PE (32-bit) | PDB          | Breakpad  |
| PE (64-bit) | PE           | Breakpad  |

### Implementation Notes

- Symbolicator downloads _all_ filetypes in parallel and filters by whether
  symbolic says that a file has debug/unwind information. There is no special
  codepath for each table presented here, and especially not for each
  platform/filetype.

  This creates a few more downloads than what would be necessary for a specific
  task, but those should not matter considering that you e.g. will likely look
  into debug information very soon if you already look for unwind information.

- As a result of the previous point, Symbolicator does not differentiate between
  32-bit and 64-bit platforms, meaning that even on 64-bit Windows it will
  attempt to look into the PDB to find relevant information. Again this should
  not matter in terms of correctness as it will still also unconditionally look
  into the PE and pick that if it's deemed to be be of better quality.

## Conversion Algorithms

Some identifiers may be computed from others. See the following list for allowed
non-lossy conversions in pseudocode:

- **MachO**: `code_id` ←→ `debug_id`. Symbolicator implements this by using the
  code ID everywhere.

- **ELF**: `code_id` → `debug_id`. This is not yet implemented.

        debug_id = code_id[0..16]

        if object.little_endian {
          debug_id[0..4].reverse(); // uuid field 1
          debug_id[4..6].reverse(); // uuid field 2
          debug_id[6..8].reverse(); // uuid field 3
        }
