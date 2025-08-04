use std::fmt::Write;
use std::sync::LazyLock;

use regex::Regex;
use sha2::{Digest, Sha256};
use symbolic::debuginfo::js::discover_sourcemaps_location;

use crate::api_lookup::ArtifactHeaders;
use crate::lookup::SourceMapUrl;

static WEBPACK_NAMESPACE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^webpack(-internal)?://[a-zA-Z0-9_\-@\.]+/\./").unwrap());

// Names that do not provide any reasonable value, and that can possibly obstruct
// better available names. In case we encounter one, we fallback to current frame fn name if available.
const USELESS_FN_NAMES: [&str; 3] = ["<anonymous>", "__webpack_require__", "__webpack_modules__"];

/// Get function name for a given frame based on the token resolved by symbolic.
/// It tries following paths in order:
/// - return token function name if we have a usable value (filtered through `USELESS_FN_NAMES` list),
/// - return mapped name of the caller (previous frame) token if it had,
/// - return token function name, including filtered values if it mapped to anything in the first place,
/// - return current frames function name as a fallback
///
// fn get_function_for_token(frame, token, previous_frame=None):
pub fn get_function_for_token<'a>(
    frame_fn_name: Option<&'a str>,
    token_fn_name: &'a str,
    callsite_fn_name: Option<&'a str>,
) -> &'a str {
    // Try to use the function name we got from sourcemap-cache, filtering useless names.
    if !USELESS_FN_NAMES.contains(&token_fn_name) {
        return token_fn_name;
    }

    // If not found, ask the callsite (previous token) for function name if possible.
    if let Some(token_name) = callsite_fn_name
        && !token_name.is_empty()
    {
        return token_name;
    }

    // If there was no minified name at all, return even useless, filtered one from the original token.
    if frame_fn_name.is_none() {
        return token_fn_name;
    }

    // Otherwise fallback to the old, minified name.
    frame_fn_name.unwrap_or("<unknown>")
}

/// Fold multiple consecutive occurences of the same property name into a single group, excluding the last component.
///
/// foo | foo
/// foo.foo | foo.foo
/// foo.foo.foo | {foo#2}.foo
/// bar.foo.foo | bar.foo.foo
/// bar.foo.foo.foo | bar.{foo#2}.foo
/// bar.foo.foo.onError | bar.{foo#2}.onError
/// bar.bar.bar.foo.foo.onError | {bar#3}.{foo#2}.onError
/// bar.foo.foo.bar.bar.onError | bar.{foo#2}.{bar#2}.onError
pub fn fold_function_name(function_name: &str) -> String {
    let mut parts: Vec<_> = function_name.split('.').collect();

    if parts.len() == 1 {
        return function_name.to_string();
    }

    // unwrap: `parts` has at least a single item.
    let tail = parts.pop().unwrap();
    let mut grouped: Vec<Vec<&str>> = vec![vec![]];

    for part in parts {
        // unwrap: we initialized `grouped` with at least a single slice.
        let current_group = grouped.last_mut().unwrap();
        if current_group.is_empty() || current_group.last() == Some(&part) {
            current_group.push(part);
        } else {
            grouped.push(vec![part]);
        }
    }

    let folded = grouped
        .iter()
        .map(|group| {
            // unwrap: each group contains at least a single item.
            if group.len() == 1 {
                group.first().unwrap().to_string()
            } else {
                format!("{{{}#{}}}", group.first().unwrap(), group.len())
            }
        })
        .collect::<Vec<_>>()
        .join(".");

    format!("{folded}.{tail}")
}

pub fn fixup_webpack_filename(filename: &str) -> String {
    if let Some((_, rest)) = filename.split_once("/~/") {
        format!("~/{rest}")
    } else if WEBPACK_NAMESPACE_RE.is_match(filename) {
        WEBPACK_NAMESPACE_RE.replace(filename, "./").to_string()
    } else if let Some(rest) = filename.strip_prefix("webpack:///") {
        rest.to_string()
    } else if let Some(rest) = filename.strip_prefix("webpack-internal:///") {
        rest.to_string()
    } else {
        filename.to_string()
    }
}

// As a running joke, here you have a 8 year old comment from 2015:
// TODO(dcramer): replace CLEAN_MODULE_RE with tokenizer completely
static CLEAN_MODULE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?ix)
^
(?:/|  # Leading slashes
(?:
    (?:java)?scripts?|js|build|static|node_modules|bower_components|[_\.~].*?|  # common folder prefixes
    v?(?:\d+\.)*\d+|   # version numbers, v1, 1.0.0
    [a-f0-9]{7,8}|     # short sha
    [a-f0-9]{32}|      # md5
    [a-f0-9]{40}       # sha1
)/)+|
(?:[-\.][a-f0-9]{7,}$)  # Ending in a commitish
",
    ).unwrap()
});

/// Converts a url into a made-up module name by doing the following:
/// * Extract just the path name ignoring querystrings
/// * Trimming off the initial /
/// * Trimming off the file extension
/// * Removes off useless folder prefixes
///
/// e.g. `http://google.com/js/v1.0/foo/bar/baz.js` -> `foo/bar/baz`
pub fn generate_module(abs_path: &str) -> String {
    let path = strip_hostname(abs_path);
    let mut path = path.split(&['#', '?']).next().unwrap_or(path);

    if let Some((idx, ".")) = path.rmatch_indices(&['.', '/']).next() {
        path = &path[..idx];
    }

    let path = path.strip_suffix(".min").unwrap_or(path);

    // return all the segments following a 32/40-char hash
    let mut segments = path.split('/');
    while let Some(segment) = segments.next() {
        if segment.len() == 32
            || segment.len() == 40 && segment.chars().all(|c| c.is_ascii_hexdigit())
        {
            let mut s = String::new();
            for (i, seg) in segments.enumerate() {
                if i > 0 {
                    s.push('/');
                }
                s.push_str(seg);
            }
            return s;
        }
    }

    CLEAN_MODULE_RE.replace_all(path, "").into_owned()
}

/// Joins the `right` path to the `base` path, taking care of our special `~/` prefix that is treated just
/// like an absolute url.
pub fn join_paths(base: &str, right: &str) -> String {
    if right.contains("://") || right.starts_with("webpack:") {
        return right.into();
    }

    let (scheme, rest) = base.split_once("://").unwrap_or(("file", base));

    let right = right.strip_prefix('~').unwrap_or(right);
    // the right path is absolute:
    if right.starts_with('/') {
        if scheme == "file" {
            return right.into();
        }
        // a leading `//` means we are skipping the hostname
        if let Some(right) = right.strip_prefix("//") {
            return format!("{scheme}://{right}");
        }
        let hostname = rest.split('/').next().unwrap_or(rest);
        return format!("{scheme}://{hostname}{right}");
    }

    let mut final_path = String::new();

    let mut left_iter = rest.split('/').peekable();
    // add the scheme/hostname
    if scheme != "file" {
        let hostname = left_iter.next().unwrap_or_default();
        write!(final_path, "{scheme}://{hostname}").unwrap();
    } else if left_iter.peek() == Some(&"") {
        // pop a leading `/`
        let _ = left_iter.next();
    }

    // pop the basename from the back
    let _ = left_iter.next_back();

    let mut segments: Vec<_> = left_iter.collect();
    let is_http = scheme == "http" || scheme == "https";
    let mut is_first_segment = true;
    for right_segment in right.split('/') {
        if right_segment == ".." && (segments.pop().is_some() || is_http) {
            continue;
        }
        if right_segment == "." && (is_http || is_first_segment) {
            continue;
        }
        is_first_segment = false;

        segments.push(right_segment);
    }

    for seg in segments {
        // FIXME: do we want to skip all the `.` fragments as well?
        if !seg.is_empty() {
            write!(final_path, "/{seg}").unwrap();
        }
    }
    final_path
}

/// Strips the hostname (or leading tilde) from the `path` and returns the path following the
/// hostname, with a leading `/`.
pub fn strip_hostname(path: &str) -> &str {
    if let Some(after_tilde) = path.strip_prefix('~') {
        return after_tilde;
    }

    if let Some((_scheme, rest)) = path.split_once("://") {
        return rest.find('/').map(|idx| &rest[idx..]).unwrap_or(rest);
    }
    path
}

/// Extracts a "file stem" from a path.
/// This is the `"/path/to/file"` in `"./path/to/file.min.js?foo=bar"`.
/// We use the most generic variant instead here, as server-side filtering is using a partial
/// match on the whole artifact path, thus `index.js` will be fetched no matter it's stored
/// as `~/index.js`, `~/index.js?foo=bar`, `http://example.com/index.js`,
/// or `http://example.com/index.js?foo=bar`.
// NOTE: We do want a leading slash to be included, eg. `/bundle/app.js` or `/index.js`,
// as it's not possible to use artifacts without proper host or `~/` wildcard.
pub fn extract_file_stem(path: &str) -> String {
    let path = strip_hostname(path);

    path.rsplit_once('/')
        .map(|(prefix, name)| {
            // trim query strings and fragments
            let name = name.split_once('?').map(|(name, _)| name).unwrap_or(name);
            let name = name.split_once('#').map(|(name, _)| name).unwrap_or(name);

            // then, trim all the suffixes as often as they occurr
            let name = trim_all_end_matches(name, FILE_SUFFIX_PATTERNS);

            format!("{prefix}/{name}")
        })
        .unwrap_or(path.to_owned())
}

const FILE_SUFFIX_PATTERNS: &[&str] = &[
    ".min", ".js", ".map", ".cjs", ".mjs", ".ts", ".d", ".jsx", ".tsx",
];

/// Trims the different `patterns` from the end of the `input` string as often as possible.
pub fn trim_all_end_matches<'a>(mut input: &'a str, patterns: &[&str]) -> &'a str {
    loop {
        let mut trimmed = input;
        for pattern in patterns {
            trimmed = trimmed.trim_end_matches(pattern);
        }
        if trimmed == input {
            return trimmed;
        }
        input = trimmed;
    }
}

/// Transforms a full absolute url into 2 or 4 generalized options.
// Based on `ReleaseFile.normalize`, see:
// https://github.com/getsentry/sentry/blob/master/src/sentry/models/releasefile.py
pub fn get_release_file_candidate_urls(url: &str) -> impl Iterator<Item = String> {
    let url = url.split('#').next().unwrap_or(url);
    let relative = strip_hostname(url);

    let urls = [
        // Absolute without fragment
        Some(url.to_string()),
        // Absolute without query
        url.split_once('?').map(|s| s.0.to_string()),
        // Relative without fragment
        Some(format!("~{relative}")),
        // Relative without query
        relative.split_once('?').map(|s| format!("~{}", s.0)),
    ];

    urls.into_iter().flatten()
}

/// Joins together frames `abs_path` and discovered sourcemap reference.
pub fn resolve_sourcemap_url(
    abs_path: &str,
    artifact_headers: &ArtifactHeaders,
    artifact_source: &str,
) -> Option<SourceMapUrl> {
    if let Some(header) = artifact_headers.get("sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else if let Some(header) = artifact_headers.get("x-sourcemap") {
        SourceMapUrl::parse_with_prefix(abs_path, header).ok()
    } else {
        let sm_ref = discover_sourcemaps_location(artifact_source)?;
        SourceMapUrl::parse_with_prefix(abs_path, sm_ref).ok()
    }
}

/// This will truncate the `timestamp` to a multiple of `refresh_every`, using a stable offset
/// derived from `url` to avoid having the same cutoff for every single `url`.
pub fn cache_busting_key(url: &str, timestamp: u64, refresh_every: u64) -> u64 {
    let url_hash = Sha256::digest(url);
    let url_hash =
        u64::from_le_bytes(<[u8; 8]>::try_from(&url_hash[..8]).expect("sha256 outputs >8 bytes"));

    let offset = url_hash % refresh_every;
    ((timestamp - offset) / refresh_every * refresh_every) + offset
}

#[cfg(test)]
mod tests {
    use symbolicator_service::caching::CacheError;

    use super::*;

    #[test]
    fn test_cache_busting_key() {
        // the hashed offset for this url is `39`
        let url = "https://example.com/foo.js";

        let timestamp = 1000;
        let refresh_every = 100;

        let key = cache_busting_key(url, timestamp, refresh_every);
        assert_eq!(key, 939);
        let key = cache_busting_key(url, timestamp + 38, refresh_every);
        assert_eq!(key, 939);
        let key = cache_busting_key(url, timestamp + 40, refresh_every);
        assert_eq!(key, 1039);
        let key = cache_busting_key(url, timestamp + 100, refresh_every);
        assert_eq!(key, 1039);
    }

    #[test]
    fn test_strip_hostname() {
        assert_eq!(strip_hostname("/absolute/unix/path"), "/absolute/unix/path");
        assert_eq!(strip_hostname("~/with/tilde"), "/with/tilde");
        assert_eq!(strip_hostname("https://example.com/"), "/");
        assert_eq!(
            strip_hostname("https://example.com/some/path/file.js"),
            "/some/path/file.js"
        );
    }

    #[test]
    fn test_get_release_file_candidate_urls() {
        let url = "https://example.com/assets/bundle.min.js";
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz";
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js#wat";
        let expected = &[
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat";
        let expected = &[
            "https://example.com/assets/bundle.min.js?foo=1&bar=baz",
            "https://example.com/assets/bundle.min.js",
            "~/assets/bundle.min.js?foo=1&bar=baz",
            "~/assets/bundle.min.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);

        let url = "app:///_next/server/pages/_error.js";
        let expected = &[
            "app:///_next/server/pages/_error.js",
            "~/_next/server/pages/_error.js",
        ];
        let actual: Vec<_> = get_release_file_candidate_urls(url).collect();
        assert_eq!(&actual, expected);
    }

    #[test]
    fn test_extract_file_stem() {
        let url = "https://example.com/bundle.js";
        assert_eq!(extract_file_stem(url), "/bundle");

        let url = "https://example.com/bundle.min.js";
        assert_eq!(extract_file_stem(url), "/bundle");

        let url = "https://example.com/assets/bundle.js";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js#wat";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        let url = "https://example.com/assets/bundle.min.js?foo=1&bar=baz#wat";
        assert_eq!(extract_file_stem(url), "/assets/bundle");

        // app:// urls
        assert_eq!(
            extract_file_stem("app:///_next/server/pages/_error.js"),
            "/_next/server/pages/_error"
        );
        assert_eq!(
            extract_file_stem("app:///polyfills.e9f8f1606b76a9c9.js"),
            "/polyfills.e9f8f1606b76a9c9"
        );
    }

    #[test]
    fn joining_paths() {
        // (http) URLs
        let base = "https://example.com/path/to/assets/bundle.min.js?foo=1&bar=baz#wat";

        // relative
        assert_eq!(
            join_paths(base, "../sourcemaps/bundle.min.js.map"),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );
        // absolute
        assert_eq!(join_paths(base, "/foo.js"), "https://example.com/foo.js");
        // absolute with tilde
        assert_eq!(join_paths(base, "~/foo.js"), "https://example.com/foo.js");

        // dots
        assert_eq!(
            join_paths(base, ".././.././to/./sourcemaps/./bundle.min.js.map"),
            "https://example.com/path/to/sourcemaps/bundle.min.js.map"
        );

        // file paths
        let base = "/home/foo/bar/baz.js";

        // relative
        assert_eq!(
            join_paths(base, "../sourcemaps/bundle.min.js.map"),
            "/home/foo/sourcemaps/bundle.min.js.map"
        );
        // absolute
        assert_eq!(join_paths(base, "/foo.js"), "/foo.js");
        // absolute with tilde
        assert_eq!(join_paths(base, "~/foo.js"), "/foo.js");

        // absolute path with its own scheme
        let path = "webpack:///../node_modules/scheduler/cjs/scheduler.production.min.js";
        assert_eq!(join_paths("http://example.com", path), path);

        // path with a dot in the middle
        assert_eq!(
            join_paths("http://example.com", "path/./to/file.min.js"),
            "http://example.com/path/to/file.min.js"
        );

        assert_eq!(
            join_paths("/playground/öut path/rollup/entrypoint1.js", "~/0.js.map"),
            "/0.js.map"
        );

        // path with a leading dot
        assert_eq!(
            join_paths(
                "app:///_next/static/chunks/pages/_app-569c402ef19f6d7b.js.map",
                "./node_modules/@sentry/browser/esm/integrations/trycatch.js"
            ),
            "app:///_next/static/chunks/pages/node_modules/@sentry/browser/esm/integrations/trycatch.js"
        );

        // webpack with only a single slash
        assert_eq!(
            join_paths(
                "app:///main-es2015.6216307eafb7335c4565.js.map",
                "webpack:/node_modules/@angular/core/__ivy_ngcc__/fesm2015/core.js"
            ),
            "webpack:/node_modules/@angular/core/__ivy_ngcc__/fesm2015/core.js"
        );

        // double-slash in the middle
        assert_eq!(
            join_paths(
                "https://foo.cloudfront.net/static//js/npm.sentry.d8b531aaf5202ddb7e90.js",
                "npm.sentry.d8b531aaf5202ddb7e90.js.map"
            ),
            "https://foo.cloudfront.net/static/js/npm.sentry.d8b531aaf5202ddb7e90.js.map"
        );

        // tests ported from python:
        // <https://github.com/getsentry/sentry/blob/ae9c0d8a33d509d9719a5a03e06c9797741877e9/tests/sentry/utils/test_urls.py#L22>
        assert_eq!(
            join_paths("http://example.com/foo", "bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo", "/bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("https://example.com/foo", "/bar"),
            "https://example.com/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo/baz", "bar"),
            "http://example.com/foo/bar"
        );
        assert_eq!(
            join_paths("http://example.com/foo/baz", "/bar"),
            "http://example.com/bar"
        );
        assert_eq!(
            join_paths("aps://example.com/foo", "/bar"),
            "aps://example.com/bar"
        );
        assert_eq!(
            join_paths("apsunknown://example.com/foo", "/bar"),
            "apsunknown://example.com/bar"
        );
        assert_eq!(
            join_paths("apsunknown://example.com/foo", "//aha/uhu"),
            "apsunknown://aha/uhu"
        );
    }

    #[test]
    fn data_urls() {
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data"),
            Ok(SourceMapUrl::Remote("/data".into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:"),
            Err(CacheError::Malformed("invalid `data:` url".into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:,foo"),
            Ok(SourceMapUrl::Data(String::from("foo").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:,Hello%2C%20World%21"),
            Ok(SourceMapUrl::Data(String::from("Hello, World!").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:;base64,SGVsbG8sIFdvcmxkIQ=="),
            Ok(SourceMapUrl::Data(String::from("Hello, World!").into())),
        );
        assert_eq!(
            SourceMapUrl::parse_with_prefix("/foo", "data:;base64,SGVsbG8sIFdvcmxkIQ="),
            Err(CacheError::Malformed("invalid `data:` url".into())),
        );
    }

    #[test]
    fn test_get_function_name_valid_name() {
        assert_eq!(
            get_function_for_token(Some("original"), "lookedup", None),
            "lookedup"
        );
    }
    #[test]
    fn test_get_function_name_fallback_to_previous_frames_token_if_useless_name() {
        assert_eq!(
            get_function_for_token(None, "__webpack_require__", Some("previous_name")),
            "previous_name"
        )
    }
    #[test]
    fn test_get_function_name_fallback_to_useless_name() {
        assert_eq!(
            get_function_for_token(None, "__webpack_require__", None),
            "__webpack_require__"
        )
    }
    #[test]
    fn test_get_function_name_fallback_to_original_name() {
        assert_eq!(
            get_function_for_token(Some("original"), "__webpack_require__", None),
            "original"
        )
    }

    #[test]
    fn test_fold_function_name() {
        assert_eq!(fold_function_name("foo"), "foo");
        assert_eq!(fold_function_name("foo.foo"), "foo.foo");
        assert_eq!(fold_function_name("foo.foo.foo"), "{foo#2}.foo");
        assert_eq!(fold_function_name("bar.foo.foo"), "bar.foo.foo");
        assert_eq!(fold_function_name("bar.foo.foo.foo"), "bar.{foo#2}.foo");
        assert_eq!(
            fold_function_name("bar.foo.foo.onError"),
            "bar.{foo#2}.onError"
        );
        assert_eq!(
            fold_function_name("bar.bar.bar.foo.foo.onError"),
            "{bar#3}.{foo#2}.onError"
        );
        assert_eq!(
            fold_function_name("bar.foo.foo.bar.bar.onError"),
            "bar.{foo#2}.{bar#2}.onError"
        );
    }

    #[test]
    fn test_fixup_webpack_filename() {
        let filename = "webpack:///../node_modules/@sentry/browser/esm/helpers.js";

        assert_eq!(
            fixup_webpack_filename(filename),
            "../node_modules/@sentry/browser/esm/helpers.js"
        );

        let filename = "webpack:///./app/utils/requestError/createRequestError.tsx";

        assert_eq!(
            fixup_webpack_filename(filename),
            "./app/utils/requestError/createRequestError.tsx"
        );

        let filename = "webpack-internal:///./src/App.jsx";
        assert_eq!(fixup_webpack_filename(filename), "./src/App.jsx");
    }

    #[test]
    fn test_generate_module() {
        assert_eq!(generate_module("http://example.com/foo.js"), "foo");
        assert_eq!(generate_module("http://example.com/foo/bar.js"), "foo/bar");
        assert_eq!(
            generate_module("http://example.com/js/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/javascript/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/1.0/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/v1/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/v1.0.0/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/_baz/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/1/2/3/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/abcdef0/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module(
                "http://example.com/92cd589eca8235e7b373bf5ae94ebf898e3b949c/foo/bar.js"
            ),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/7d6d00eae0ceccdc7ee689659585d95f/foo/bar.js"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/foo/bar.coffee"),
            "foo/bar"
        );
        assert_eq!(
            generate_module("http://example.com/foo/bar.js?v=1234"),
            "foo/bar"
        );
        assert_eq!(generate_module("/foo/bar.js"), "foo/bar");
        assert_eq!(generate_module("/foo/bar.ts"), "foo/bar");
        assert_eq!(generate_module("../../foo/bar.js"), "foo/bar");
        assert_eq!(generate_module("../../foo/bar.ts"), "foo/bar");
        assert_eq!(generate_module("../../foo/bar.awesome"), "foo/bar");
        assert_eq!(generate_module("../../foo/bar"), "foo/bar");
        assert_eq!(
            generate_module("/foo/bar-7d6d00eae0ceccdc7ee689659585d95f.js"),
            "foo/bar"
        );
        assert_eq!(generate_module("/bower_components/foo/bar.js"), "foo/bar");
        assert_eq!(generate_module("/node_modules/foo/bar.js"), "foo/bar");
        assert_eq!(
            generate_module(
                "http://example.com/vendor.92cd589eca8235e7b373bf5ae94ebf898e3b949c.js",
            ),
            "vendor",
        );
        assert_eq!(
            generate_module(
                "/a/javascripts/application-bundle-149360d3414c26adac3febdf6832e25c.min.js"
            ),
            "a/javascripts/application-bundle"
        );
        assert_eq!(
            generate_module("https://example.com/libs/libs-20150417171659.min.js"),
            "libs/libs"
        );
        assert_eq!(
            generate_module("webpack:///92cd589eca8235e7b373bf5ae94ebf898e3b949c/vendor.js"),
            "vendor"
        );
        assert_eq!(
            generate_module("webpack:///92cd589eca8235e7b373bf5ae94ebf898e3b949c/vendor.js"),
            "vendor"
        );
        assert_eq!(
            generate_module("app:///92cd589eca8235e7b373bf5ae94ebf898e3b949c/vendor.js"),
            "vendor"
        );
        assert_eq!(
            generate_module("app:///example/92cd589eca8235e7b373bf5ae94ebf898e3b949c/vendor.js"),
            "vendor"
        );
        assert_eq!(
            generate_module("~/app/components/projectHeader/projectSelector.jsx"),
            "app/components/projectHeader/projectSelector"
        );
    }
}
