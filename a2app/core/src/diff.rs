//! Line diffs, for showing what changed between two versions of an app's source.

/// One line of a diff, in output order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiffLine {
    Context(String),
    Added(String),
    Removed(String),
}

/// Diffs `old` against `new` line by line. Within a changed hunk, removed
/// lines come before added ones; identical inputs are all `Context`.
pub fn line_diff(old: &str, new: &str) -> Vec<DiffLine> {
    let old: Vec<&str> = old.lines().collect();
    let new: Vec<&str> = new.lines().collect();
    let (n, m) = (old.len(), new.len());
    let w = m + 1;

    // lcs[i * w + j] is the LCS length of old[..i] and new[..j].
    let mut lcs = vec![0u32; (n + 1) * w];
    for i in 1..=n {
        for j in 1..=m {
            lcs[i * w + j] = if old[i - 1] == new[j - 1] {
                lcs[(i - 1) * w + j - 1] + 1
            } else {
                lcs[(i - 1) * w + j].max(lcs[i * w + j - 1])
            };
        }
    }

    // Walk back from the end, so the diff comes out reversed. Preferring the
    // added side on ties is what puts removals first once we flip it.
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (n, m);
    while i > 0 || j > 0 {
        if i > 0 && j > 0 && old[i - 1] == new[j - 1] {
            i -= 1;
            j -= 1;
            out.push(DiffLine::Context(old[i].to_string()));
        } else if j > 0 && (i == 0 || lcs[i * w + j - 1] >= lcs[(i - 1) * w + j]) {
            j -= 1;
            out.push(DiffLine::Added(new[j].to_string()));
        } else {
            i -= 1;
            out.push(DiffLine::Removed(old[i].to_string()));
        }
    }
    out.reverse();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(s: &str) -> DiffLine { DiffLine::Context(s.to_string()) }
    fn add(s: &str) -> DiffLine { DiffLine::Added(s.to_string()) }
    fn del(s: &str) -> DiffLine { DiffLine::Removed(s.to_string()) }

    #[test]
    fn identical_is_all_context() {
        assert_eq!(line_diff("a\nb\nc", "a\nb\nc"), vec![ctx("a"), ctx("b"), ctx("c")]);
    }

    #[test]
    fn pure_insertion() {
        assert_eq!(line_diff("a\nc", "a\nb\nc"), vec![ctx("a"), add("b"), ctx("c")]);
        assert_eq!(line_diff("a", "a\nb\nc"), vec![ctx("a"), add("b"), add("c")]);
    }

    #[test]
    fn pure_removal() {
        assert_eq!(line_diff("a\nb\nc", "a\nc"), vec![ctx("a"), del("b"), ctx("c")]);
        assert_eq!(line_diff("a\nb\nc", "c"), vec![del("a"), del("b"), ctx("c")]);
    }

    #[test]
    fn replacement_lists_removed_before_added() {
        assert_eq!(
            line_diff("a\nx\ny\nd", "a\nb\nc\nd"),
            vec![ctx("a"), del("x"), del("y"), add("b"), add("c"), ctx("d")],
        );
        assert_eq!(line_diff("x", "y"), vec![del("x"), add("y")]);
    }

    #[test]
    fn empty_old_is_all_added() {
        assert_eq!(line_diff("", "a\nb"), vec![add("a"), add("b")]);
    }

    #[test]
    fn empty_new_is_all_removed() {
        assert_eq!(line_diff("a\nb", ""), vec![del("a"), del("b")]);
        assert!(line_diff("", "").is_empty());
    }

    #[test]
    fn trailing_newline_is_not_a_line() {
        assert_eq!(line_diff("a\nb\n", "a\nb"), vec![ctx("a"), ctx("b")]);
        assert_eq!(line_diff("a\r\nb\r\n", "a\nb"), vec![ctx("a"), ctx("b")]);
        assert_eq!(line_diff("a\n", "a\n\n"), vec![ctx("a"), add("")]);
    }
}
