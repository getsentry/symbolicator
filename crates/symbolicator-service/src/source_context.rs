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

    #[test]
    fn test_get_context_lines() {
        let source = "\
Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Sit amet mattis vulputate enim nulla aliquet porttitor. Dis parturient montes nascetur ridiculus mus mauris vitae ultricies. Eget lorem dolor sed viverra ipsum. Tempor id eu nisl nunc mi ipsum faucibus vitae aliquet. Aliquam etiam erat velit scelerisque in dictum non. Blandit aliquam etiam erat velit. Maecenas sed enim ut sem viverra. Facilisi nullam vehicula ipsum a arcu cursus vitae. Non enim praesent elementum facilisis leo vel. Egestas congue quisque egestas diam in arcu cursus euismod quis. Malesuada fames ac turpis egestas maecenas. Non quam lacus suspendisse faucibus interdum posuere lorem ipsum. Felis eget nunc lobortis mattis aliquam faucibus purus in massa. Posuere morbi leo urna molestie at elementum eu facilisis sed. Cras ornare arcu dui vivamus arcu felis bibendum ut. Nibh tortor id aliquet lectus proin nibh nisl condimentum id.

Netus et malesuada fames ac turpis egestas. Id eu nisl nunc mi ipsum faucibus vitae aliquet nec. Adipiscing commodo elit at imperdiet dui accumsan sit amet. Dui id ornare arcu odio ut sem. In nisl nisi scelerisque eu. Consequat semper viverra nam libero. Lacus laoreet non curabitur gravida arcu ac tortor dignissim convallis. At augue eget arcu dictum. Ac turpis egestas maecenas pharetra. Nisi est sit amet facilisis magna etiam tempor orci. Eget arcu dictum varius duis. Nunc lobortis mattis aliquam faucibus purus in massa tempor. Id leo in vitae turpis. Et malesuada fames ac turpis egestas. Dictum fusce ut placerat orci nulla pellentesque. Cras pulvinar mattis nunc sed blandit libero volutpat sed cras.

Consequat ac felis donec et odio pellentesque diam volutpat commodo. Pulvinar elementum integer enim neque volutpat ac. Mollis aliquam ut porttitor leo a diam sollicitudin. Velit ut tortor pretium viverra suspendisse potenti nullam. Viverra tellus in hac habitasse platea dictumst vestibulum rhoncus. Dictum non consectetur a erat nam at lectus urna duis. Condimentum vitae sapien pellentesque habitant morbi tristique senectus et. Tempus urna et pharetra pharetra massa massa ultricies. Integer malesuada nunc vel risus commodo viverra maecenas. Neque viverra justo nec ultrices dui sapien. Fermentum leo vel orci porta non. A diam maecenas sed enim ut sem viverra aliquet eget. Tincidunt ornare massa eget egestas purus. Curabitur gravida arcu ac tortor dignissim convallis aenean.
";

        // trim to column 0
        let (before, line, after) = get_context_lines(source, 3, 0, Some(2)).unwrap();
        assert_eq!(before, vec!["Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Sit amet mattis  {snip}", ""]);
        assert_eq!(line, "Netus et malesuada fames ac turpis egestas. Id eu nisl nunc mi ipsum faucibus vitae aliquet nec. Adipiscing commodo elit at imperdiet dui ac {snip}");
        assert_eq!(after, vec!["", "Consequat ac felis donec et odio pellentesque diam volutpat commodo. Pulvinar elementum integer enim neque volutpat ac. Mollis aliquam ut po {snip}"]);

        // trim to column 500
        let (before, line, after) = get_context_lines(source, 3, 500, Some(2)).unwrap();
        assert_eq!(before, vec!["{snip} enim ut sem viverra. Facilisi nullam vehicula ipsum a arcu cursus vitae. Non enim praesent elementum facilisis leo vel. Egestas congue quisq {snip}", ""]);
        assert_eq!(line, "{snip} ci. Eget arcu dictum varius duis. Nunc lobortis mattis aliquam faucibus purus in massa tempor. Id leo in vitae turpis. Et malesuada fames ac {snip}");
        assert_eq!(after, vec!["", "{snip} rna et pharetra pharetra massa massa ultricies. Integer malesuada nunc vel risus commodo viverra maecenas. Neque viverra justo nec ultrices  {snip}"]);

        // trim to column 1000
        let (before, line, after) = get_context_lines(source, 3, 1000, Some(2)).unwrap();
        assert_eq!(before, vec!["{snip} ementum eu facilisis sed. Cras ornare arcu dui vivamus arcu felis bibendum ut. Nibh tortor id aliquet lectus proin nibh nisl condimentum id.", ""]);
        assert_eq!(line, "{snip} a fames ac turpis egestas. Dictum fusce ut placerat orci nulla pellentesque. Cras pulvinar mattis nunc sed blandit libero volutpat sed cras.");
        assert_eq!(after, vec!["", "{snip} ed enim ut sem viverra aliquet eget. Tincidunt ornare massa eget egestas purus. Curabitur gravida arcu ac tortor dignissim convallis aenean."]);
    }
}
