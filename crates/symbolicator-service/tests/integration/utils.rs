/// Helper to redact the port number from localhost URIs in insta snapshots.
///
/// Since we use a localhost source on a random port during tests we get random port
/// numbers in URI of the dif object file candidates.  This redaction masks this out.
pub fn redact_localhost_port(
    value: insta::internals::Content,
    _path: insta::internals::ContentPath<'_>,
) -> impl Into<insta::internals::Content> {
    let re = regex::Regex::new(r"^http://localhost:[0-9]+").unwrap();
    re.replace(value.as_str().unwrap(), "http://localhost:<port>")
        .into_owned()
}

#[macro_export]
macro_rules! assert_snapshot {
    ($e:expr) => {
        ::insta::assert_yaml_snapshot!($e, {
            ".**.location" => ::insta::dynamic_redaction(
                $crate::utils::redact_localhost_port
            )
        });
    }
}
