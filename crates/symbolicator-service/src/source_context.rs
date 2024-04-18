pub const DEFAULT_CONTEXT_LINES: usize = 5;

/// Resolves the source context around `lineno` from the `source` file.
///
/// N (`context_lines`) lines of context will be resolved in addition to `lineno`.
/// This defaults to [`DEFAULT_CONTEXT_LINES`].
/// Every output line will be truncated to ~150 characters, centered as best as possible
/// around `trim_to_column`, and a `{snip}` marker is added at the trimmed edges.
/// To simply cut off lines after ~150 characters, pass 0.
///
/// If no source line for `lineno` is found in `source`, it will return [`None`] instead.
pub fn get_context_lines(
    source: &str,
    lineno: usize,
    trim_to_column: usize,
    context_lines: Option<usize>,
) -> Option<(Vec<String>, String, Vec<String>)> {
    let context_lines = context_lines.unwrap_or(DEFAULT_CONTEXT_LINES);

    let start_line = lineno.saturating_sub(context_lines).saturating_sub(1);
    let line_diff = (lineno - start_line).saturating_sub(1);

    let trim_line = |line: &str| trim_context_line(line, trim_to_column);

    let mut lines = source.lines().skip(start_line);
    let pre_context = (&mut lines).take(line_diff).map(trim_line).collect();
    let context = trim_line(lines.next()?);
    let post_context = lines.take(context_lines).map(trim_line).collect();

    Some((pre_context, context, post_context))
}

/// Trims a line down to a goal of 140 characters, with a little wiggle room to be sensible
/// and tries to trim around the given `column`. So it tries to extract 60 characters
/// before the provided `column` and fill the rest up to 140 characters to yield a better context.
fn trim_context_line(line: &str, column: usize) -> String {
    let len = line.len();

    if len <= 150 {
        return line.to_string();
    }

    let col = std::cmp::min(column, len);

    let mut start = col.saturating_sub(60);
    // Round down if it brings us close to the edge.
    if start < 5 {
        start = 0;
    }

    let mut end = std::cmp::min(start + 140, len);
    // Round up to the end if it's close.
    if end > len.saturating_sub(5) {
        end = len;
    }

    // If we are bumped all the way to the end, make sure we still get a full 140 chars in the line.
    if end == len {
        start = end.saturating_sub(140);
    }

    start = floor_char_boundary(line, start);
    end = floor_char_boundary(line, end);
    let line = &line[start..end];

    let mut line = if start > 0 {
        // We've snipped from the beginning.
        format!("{{snip}} {line}")
    } else {
        line.to_string()
    };

    if end < len {
        // We've snipped from the end.
        line.push_str(" {snip}");
    }

    line
}

// This is a copy from the std lib, as the fn is nightly-only:
// <https://github.com/rust-lang/rust/blob/5fe3528be5ef12be3d12c7a9ee1b0bff9e3b35e4/library/core/src/str/mod.rs#L258>
fn floor_char_boundary(s: &str, index: usize) -> usize {
    if index >= s.len() {
        s.len()
    } else {
        let lower_bound = index.saturating_sub(3);
        let new_index = s.as_bytes()[lower_bound..=index]
            .iter()
            // This is bit magic equivalent to: b < 128 || b >= 192
            .rposition(|b| (*b as i8) >= -0x40);

        // SAFETY: we know that the character boundary will be within four bytes
        unsafe { lower_bound + new_index.unwrap_unchecked() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trim_context_line() {
        let long_line = "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.";
        assert_eq!(trim_context_line("foo", 0), "foo".to_string());
        assert_eq!(trim_context_line(long_line, 0), "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it li {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 10), "The public is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it li {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 66), "{snip} blic is more familiar with bad design than good design. It is, in effect, conditioned to prefer bad design, because that is what it lives wi {snip}".to_string());
        assert_eq!(trim_context_line(long_line, 190), "{snip} gn. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.".to_string());
        assert_eq!(trim_context_line(long_line, 9999), "{snip} gn. It is, in effect, conditioned to prefer bad design, because that is what it lives with. The new becomes threatening, the old reassuring.".to_string());

        let long_line_with_unicode = ".❤️🧡💛💚💙💜".repeat(10);
        assert_eq!(
            trim_context_line(&long_line_with_unicode, 70),
            "{snip} 🧡💛💚💙💜.❤️🧡💛💚💙💜.❤️🧡💛💚💙💜.❤️🧡💛💚💙💜.❤️🧡💛💚💙💜.❤️🧡💛 {snip}"
        );
    }
}
