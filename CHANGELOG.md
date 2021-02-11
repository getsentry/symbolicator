# Changelog

## 0.3.3

### Features

- Add NotFound status for sources which didn't provide any DIF object candidates. ([#327](https://github.com/getsentry/symbolicator/pull/327))
- Symbolication responses include download, debug and unwind information about all DIF object files which were looked up on the available object sources. ([#309](https://github.com/getsentry/symbolicator/pull/309) [#316](https://github.com/getsentry/symbolicator/pull/316) [#324](https://github.com/getsentry/symbolicator/pull/324) [#344](https://github.com/getsentry/symbolicator/pull/344))

### Bug Fixes

- Silently ignore unknown fields in minidump and apple crash report requests instead of responding with `400 Bad Request`. ([#321](https://github.com/getsentry/symbolicator/pull/321))
- Update breakpad to allow processing MIPS minidumps and support reading MIPS debug sections. ([#325](https://github.com/getsentry/symbolicator/pull/325))
- Improve amd64 stack scanning by excluding some false-positive frames. ([#325](https://github.com/getsentry/symbolicator/pull/325))
- Resolve correct line information for large inline functions.

### Internal

- Changed the internal HTTP transport layer. There is no expected difference in the way Symbolicator downloads files from symbol servers, although some internal timeouts and error messages may change. ([#335](https://github.com/getsentry/symbolicator/pull/335))
- Report more descriptive errors if symbolication fails with internal errors, timeouts or crashes. ([#365](https://github.com/getsentry/symbolicator/pull/365))
- Remove an unused threadpool. ([#370](https://github.com/getsentry/symbolicator/pull/370), [#378](https://github.com/getsentry/symbolicator/pull/378))

## 0.3.2

### Bug Fixes

- Ensure deserialisation works for objects passed through procspawn. ([#314](https://github.com/getsentry/symbolicator/pull/314))

### Tools

- `wasm-split` now retains all sections. ([#311](https://github.com/getsentry/symbolicator/pull/311))

### Features

- Symbolicator will now track Release Health Sessions for each symbolication request. ([#289](https://github.com/getsentry/symbolicator/pull/289))

## 0.3.1

### Features

- Add support for DWARF in WASM. ([#301](https://github.com/getsentry/symbolicator/pull/301))
- Support symbolication of native Dart stack traces by prioritizing DWARF function names over symbol table entries.

### Tools

- Add `wasm-split`, which splits WASM files into code and debug information. ([#303](https://github.com/getsentry/symbolicator/pull/303))

## 0.3.0

### Features

- Publish Docker containers to DockerHub at `getsentry/symbolicator`. ([#271](https://github.com/getsentry/symbolicator/pull/271))
- Add `processing_pool_size` configuration option that allows to set the size of the processing pool. ([#273](https://github.com/getsentry/symbolicator/pull/273))
- Use a dedicated `tmp` sub-directory in the cache directory to write temporary files into. ([#265](https://github.com/getsentry/symbolicator/pull/265))
- Use STATSD_SERVER environment variable to set metrics.statsd configuration option ([#182](https://github.com/getsentry/symbolicator/pull/182))
- Added WASM support. ([#301](https://github.com/getsentry/symbolicator/pull/301))

### Bug Fixes

- Fix a bug where Sentry requests were cached even though caches were disabled. ([#260](https://github.com/getsentry/symbolicator/pull/260))
- Fix CFI cache status in cases where the PDB file was found but not the PE file. ([#279](https://github.com/getsentry/symbolicator/pull/279))
- Instead of marking unused unwind info as missing, report it as unused. ([#293](https://github.com/getsentry/symbolicator/pull/293))

## 0.2.0

- This changelog is entirely incomplete, future releases will try to improve this.

### Breaking Changes

- The configuration file is stricter on which fields can be present, unknown fields will no longer be accepted and cause an error.
- Configuring cache durations is now done using (humantime)[https://docs.rs/humantime/latest/humantime/fn.parse_duration.html].

## 0.1.0

- Initial version of symbolicator
