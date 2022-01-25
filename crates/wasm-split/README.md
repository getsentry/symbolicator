# wasm-split

This tool takes a WebAssembly file and postprocesses it so it can be used with
symbolicator.  It does the following operations:

* It embeds a `build_id` custom section if such a section was not embedded yet.
* It optionally removes debug sections from a WASM file.
* It can optionally create a secondary WASM container with debug sections.

## Compiling

wasm-split is compiled for some platforms with each symbolicator release and
can be found on the [GitHub release list](https://github.com/getsentry/symbolicator/releases/).

For other platforms or for the development version, wasm-split can be compiled
with a recent rust compiler:

```
cargo build --release
```

## Examples

Add a missing build-id and modify the file in place:

```
$ wasm-split input.wasm
```

Add a missing build-id, strip debug sections from a wasm file and write to new file:

```
$ wasm-split input.wasm -o output.wasm --strip
```

Add a missing build-id, strip debug sections from a wasm file and save in separate debug file:

```
$ wasm-split input.wasm -o output.wasm --strip --debug-out=output.debug
```

In both cases a `build_id` will be added if missing and in all cases the build
ID is written to stdout in hexadecimal format.


## References

The `build_id` section is a proposed extension to WASM build tools:
[tool-conventions#133](https://github.com/WebAssembly/tool-conventions/issues/133).
