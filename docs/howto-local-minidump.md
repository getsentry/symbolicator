# HOWTO: Symbolicate a local minidump with `symbolicli`

## Prerequisites

1. **Rust toolchain** installed (for building).
2. A **minidump file** (e.g., `linux.dmp`, `windows.dmp`, or your own `.dmp`).
3. A **symbol source** configured so `symbolicli` can resolve addresses to function names.

## 1. Configure symbol sources

Edit `symbolicli.toml` (or `~/.symboliclirc`) and point it at a directory with debug symbols.

Example using a local `debuginfod` cache:

```toml
cache_dir = "cache"

[[sources]]
id = "local-debuginfod-cache"
type = "filesystem"
path = "~/.cache/debuginfod_client"
layout = { type = "debuginfod" }
```

Example using the local `symbols/` directory (unified layout):

```toml
cache_dir = "cache"

[[sources]]
id = "local-symbols"
type = "filesystem"
path = "./symbols"
layout = { type = "unified" }
```

You can also add symbols on the fly with `--symbols /path/to/sorted/dir`.

## 2. Build (if not already built)

```bash
cd symbolicator

# Debug build (faster compilation)
cargo build --package symbolicli

# Release build (faster execution, better for timing)
cargo build --release --package symbolicli
```

## 3. Run symbolication

```bash
cd symbolicator

# Basic: output JSON to stdout, stderr (logs) to a file
./target/release/symbolicli --offline --format json minidump.dmp > /tmp/result.json 2>/tmp/result.log

# Or with more verbose logging
./target/release/symbolicli --offline --format json --log-level debug minidump.dmp > /tmp/result.json 2>/tmp/result.log

# Use a specific config file (default is symbolicli.toml in CWD)
./target/release/symbolicli --config symbolicli.toml --offline --format json tests/fixtures/linux.dmp > /tmp/linux_result.json 2>&1
```

### Key flags

| Flag | Description |
|---|---|
| `--offline` | Don't contact Sentry; use only local sources. Required if you have no auth token or running instance. |
| `--format json` | Output the symbolicated result as JSON (default is a human-readable summary). |
| `--log-level <level>` | Set verbosity: `off`, `error`, `warn`, `info`, `debug`, `trace`. |
| `--symbols <dir>` | Add an extra local symbol directory (must be unified layout). |
| `--config <file>` | Use a specific config file instead of the default. |

## 4. Inspect the JSON output

### Quick summary in Python

```python
import json

with open('/tmp/result.json') as f:
    data = json.load(f)

# Print OS and crash info
print("OS:", data.get("operating_system"))
print("Crash reason:", data.get("crash_reason"))
print("Number of stacktraces:", len(data.get("stacktraces", [])))

# Show the crashing thread's top frames
for st in data['stacktraces']:
    if not st.get('is_requesting'):
        continue
    print(f"\n--- Crashing thread ({st['register_info'].get('instruction_addr', '?')}) ---")
    for i, fr in enumerate(st['frames'][:10]):
        print(f"  [{i}] {fr.get('function', '<unknown>')} @ {fr.get('package', '<unknown>')}")
```

### Inspect function arguments (e.g., `this` pointer)

```python
import json

with open('/tmp/result.json') as f:
    data = json.load(f)

for st in data['stacktraces']:
    if not st.get('is_requesting'):
        continue
    fr = st['frames'][0]
    print("function:", fr.get('function'))
    print("arguments:", fr.get('arguments'))
    break
```

### Pretty-print the full JSON

```bash
python3 -m json.tool /tmp/result.json | less -R
```

## Notes

- **Offline mode** only symbolicates native crashes. JavaScript/ProGuard symbolication requires a Sentry instance.
- If functions show as `<unknown>`, your symbol source doesn't have the right debug files. Check the log (`2>/tmp/result.log`) for download/cache misses.
- The first run populates the cache (under `cache_dir`). Subsequent runs on the same minidump will be faster.
