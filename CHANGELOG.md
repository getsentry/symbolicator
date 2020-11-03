# Changelog

## Unreleased

### Features

- Publish Docker containers to DockerHub at `getsentry/symbolicator`. ([#271](https://github.com/getsentry/symbolicator/pull/271))
- Add `processing_pool_size` configuration option that allows to set the size of the processing pool. ([#273](https://github.com/getsentry/symbolicator/pull/273))
- Use a dedicated `tmp` sub-directory in the cache directory to write temporary files into. ([#265](https://github.com/getsentry/symbolicator/pull/265))

### Bug Fixes

- Fix a bug where Sentry requests were cached even though caches were disabled. ([#260](https://github.com/getsentry/symbolicator/pull/260))
- Fix CFI cache status in cases where the PDB file was found but not the PE file. ([#279](https://github.com/getsentry/symbolicator/pull/279))

## 0.2.0

- This changelog is entirely incomplete, future releases will try to improve this.

### Breaking Changes

- The configuration file is stricter on which fields can be present, unknown fields will no longer be accepted and cause an error.
- Configuring cache durations is now done using (humantime)[https://docs.rs/humantime/latest/humantime/fn.parse_duration.html].

## 0.1.0

- Initial version of symbolicator
