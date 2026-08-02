//! Source text counted at build time.
//!
//! A rule a file states in a comment holds only until someone edits the file
//! without reading the comment. Counting the file's own source in a `const
//! fn` turns the rule into a build error instead, in every runner, including
//! the ones that give each test a process of its own.

/// Lines of `src` that are exactly `needle` once indentation is stripped.
/// Exact rather than a substring, so a doc comment naming the needle does
/// not count as one of them.
#[must_use]
pub const fn lines_equal(src: &str, needle: &str) -> usize {
    let (src, needle) = (src.as_bytes(), needle.as_bytes());
    let (mut found, mut at) = (0, 0);
    while at <= src.len() {
        let mut end = at;
        while end < src.len() && src[end] != b'\n' {
            end += 1;
        }
        let mut from = at;
        while from < end && (src[from] == b' ' || src[from] == b'\t') {
            from += 1;
        }
        let mut to = end;
        while to > from && (src[to - 1] == b' ' || src[to - 1] == b'\t' || src[to - 1] == b'\r') {
            to -= 1;
        }
        if to - from == needle.len() {
            let mut i = 0;
            while i < needle.len() && src[from + i] == needle[i] {
                i += 1;
            }
            if i == needle.len() {
                found += 1;
            }
        }
        at = end + 1;
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_whole_lines_count() {
        let src = "#[test]\n    #[test]\t\n// #[test]\n#[tokio::test]\n#[test] fn x\n";
        assert_eq!(lines_equal(src, "#[test]"), 2, "indentation is stripped");
        assert_eq!(lines_equal(src, "#[tokio::test]"), 1);
        assert_eq!(
            lines_equal("no trailing newline\n#[test]", "#[test]"),
            1,
            "the last line counts without a newline after it"
        );
        assert_eq!(lines_equal("", "#[test]"), 0);
    }
}
