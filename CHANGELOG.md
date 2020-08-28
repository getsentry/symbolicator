# Changelog

## Unreleased

* This changelog is entirely incomplete, future releases will try to
  improve this.

### Breaking Changes

* The configuration file is stricter on which fields can be present,
  unknown fields will no longer be accepted and cause an error.
* Configuring cache durations is now done using
  (humantime)[https://docs.rs/humantime/latest/humantime/fn.parse_duration.html].


## 0.1.0

* Initial version of symbolicator
