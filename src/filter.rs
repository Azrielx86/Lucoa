use std::ops::Range;
use std::sync::Arc;

use regex::{Regex, RegexBuilder};

#[derive(Clone, Debug)]
pub struct FilterSpec {
    pub raw: String,
    pub is_regex: bool,
    pub case_insensitive: bool,
}

pub struct CompiledFilter {
    re: Regex,
}

impl CompiledFilter {
    /// Literal input is escaped so both modes share one linear-time engine.
    /// Case-insensitivity lives in the regex flags: matching happens on the
    /// original string, so returned byte ranges are directly usable for span
    /// styling (lowercasing the haystack would shift offsets under Unicode
    /// case mapping).
    pub fn compile(spec: &FilterSpec) -> Result<Self, regex::Error> {
        let pattern = if spec.is_regex {
            spec.raw.clone()
        } else {
            regex::escape(&spec.raw)
        };
        let re = RegexBuilder::new(&pattern)
            .case_insensitive(spec.case_insensitive)
            .build()?;
        Ok(CompiledFilter { re })
    }

    pub fn is_match(&self, s: &str) -> bool {
        self.re.is_match(s)
    }

    /// Non-overlapping match byte ranges for span highlighting. Zero-width
    /// matches (e.g. the pattern `a*`) are skipped — there is nothing to style.
    pub fn ranges(&self, s: &str) -> Vec<Range<usize>> {
        self.re
            .find_iter(s)
            .map(|m| m.range())
            .filter(|r| !r.is_empty())
            .collect()
    }
}

/// Re-match a buffer snapshot. Runs inside `spawn_blocking`, never on the UI
/// task; the `Arc<str>` snapshot makes taking it cheap.
pub fn recompute(lines: &[Arc<str>], filter: &CompiledFilter) -> Vec<bool> {
    lines.iter().map(|l| filter.is_match(l)).collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn compile(raw: &str, is_regex: bool, ci: bool) -> CompiledFilter {
        CompiledFilter::compile(&FilterSpec {
            raw: raw.into(),
            is_regex,
            case_insensitive: ci,
        })
        .unwrap()
    }

    #[test]
    fn literal_mode_escapes_metacharacters() {
        let f = compile("a.b*", false, false);
        assert!(f.is_match("xx a.b* yy"));
        assert!(!f.is_match("aXbbb")); // would match if treated as a regex
    }

    #[test]
    fn regex_mode_and_case_insensitive() {
        let f = compile("er+or", true, true);
        assert!(f.is_match("ERRROR: boom"));
        assert!(!f.is_match("eor"));
        let cs = compile("error", true, false);
        assert!(!cs.is_match("ERROR"));
    }

    #[test]
    fn invalid_regex_is_an_error() {
        assert!(
            CompiledFilter::compile(&FilterSpec {
                raw: "[unclosed".into(),
                is_regex: true,
                case_insensitive: false,
            })
            .is_err()
        );
    }

    #[test]
    fn ranges_are_byte_offsets_and_skip_zero_width() {
        let f = compile("l+", true, false);
        assert_eq!(f.ranges("hello world"), vec![2..4, 9..10]);
        // zero-width matches produce no ranges
        let z = compile("x*", true, false);
        assert!(z.ranges("abc").is_empty());
        // multi-byte prefix: ranges stay valid byte offsets on char boundaries
        let u = compile("é", false, false);
        assert_eq!(u.ranges("aéb"), vec![1..3]);
    }

    #[test]
    fn case_insensitive_ranges_use_original_string() {
        let f = compile("err", false, true);
        let s = "an ERRor here";
        let r = f.ranges(s);
        assert_eq!(&s[r[0].clone()], "ERR");
    }
}
