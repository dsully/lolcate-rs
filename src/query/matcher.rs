use std::str;

use regex::{Regex, RegexBuilder, bytes};
use regex_syntax::hir::{
    Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind, Literal,
    Look,
};

use super::{Matches, prefilter::FoldedLiteral};

#[derive(Clone)]
pub struct Matcher {
    line: Regex,
    /// A pattern that, run over a whole newline-separated buffer, finds a superset of the lines
    /// `line` matches.
    buffer: Option<String>,
    /// A literal in every match of `buffer`, if it has a case-insensitive one.
    literal: Option<FoldedLiteral>,
}

impl Matcher {
    #[must_use]
    pub fn new(pattern: &str, ignore_case: bool) -> Self {
        Self::with_buffer_pattern(pattern, pattern, ignore_case)
    }

    /// Matches `pattern` against the basename only.
    #[must_use]
    pub fn basename(pattern: &str, ignore_case: bool) -> Self {
        let line = format!("/[^/]*{pattern}[^/]*$");
        // Dropping the leading `/[^/]*` keeps a superset of matches and lets the regex engine
        // prefilter on the pattern's literals instead of on every `/`.
        if pattern.starts_with(['*', '+', '?', '{']) {
            Self::new(&line, ignore_case)
        } else {
            Self::with_buffer_pattern(&line, &format!("{pattern}[^/]*$"), ignore_case)
        }
    }

    fn with_buffer_pattern(pattern: &str, buffer_pattern: &str, ignore_case: bool) -> Self {
        let case_insensitive = ignore_case || !pattern.bytes().any(|b| b.is_ascii_uppercase());

        let line = match RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(re) => re,
            Err(e) => panic!("invalid regex: {e}"),
        };

        let (buffer, literal) = Self::buffer_regex(buffer_pattern, case_insensitive).unzip();
        Self {
            line,
            buffer,
            literal: literal.flatten(),
        }
    }

    // Parsed in multi-line mode with `\n` removed from every class and line anchors made CRLF-aware,
    // so a buffer match never spans lines and any per-line match is a buffer match at the same
    // offset. `\A`/`\z` have no such equivalent, so those patterns use line mode.
    fn buffer_regex(
        pattern: &str,
        case_insensitive: bool,
    ) -> Option<(String, Option<FoldedLiteral>)> {
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .build()
            .parse(pattern)
            .ok()?;
        let looks = hir.properties().look_set();
        if looks.contains(Look::Start) || looks.contains(Look::End) {
            return None;
        }
        let hir = exclude_newline(hir)?;
        Some((hir.to_string(), FoldedLiteral::from_hir(&hir)))
    }

    fn push_if_match(&self, chunk: &[u8], start: usize, end: usize, out: &mut Matches) {
        let mut line = &chunk[start..end];
        if end < chunk.len() && line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = str::from_utf8(line).expect("Invalid UTF-8");

        let mut matches = self.line.find_iter(line).peekable();
        if matches.peek().is_none() {
            return;
        }

        let highlighted = &mut out.bytes;
        let first = highlighted.len();
        let mut last_end = 0;

        for mat in matches {
            highlighted.extend_from_slice(&line.as_bytes()[last_end..mat.start()]);
            highlighted.extend_from_slice(b"\x1b[31m");
            highlighted.extend_from_slice(mat.as_str().as_bytes());
            highlighted.extend_from_slice(b"\x1b[0m");
            last_end = mat.end();
        }

        highlighted.extend_from_slice(&line.as_bytes()[last_end..]);

        out.lines.push(first..highlighted.len());
    }
}

/// The query's patterns; a line is output once for each pattern it matches.
#[derive(Clone)]
pub struct Patterns {
    matchers: Vec<Matcher>,
    /// Finds a superset of the lines any matcher matches, in one pass over a buffer; `None` if
    /// some matcher has to see every line.
    buffer: Option<bytes::Regex>,
    /// A lone pattern's case-insensitive literal, which finds candidate lines faster than `buffer`.
    literal: Option<FoldedLiteral>,
}

impl Patterns {
    #[must_use]
    pub fn new(matchers: Vec<Matcher>) -> Self {
        let buffer = matchers
            .iter()
            .map(|m| m.buffer.as_deref().map(|p| format!("(?:{p})")))
            .collect::<Option<Vec<_>>>()
            .and_then(|alternatives| bytes::Regex::new(&alternatives.join("|")).ok());
        let literal = match matchers.as_slice() {
            [matcher] => matcher.literal.clone(),
            _ => None,
        };
        Self {
            matchers,
            buffer,
            literal,
        }
    }

    /// Pushes every line of `chunk` matching each pattern, highlighted, onto `out`.
    pub fn search(&self, chunk: &[u8], out: &mut Matches) {
        if self.matchers.is_empty() {
            return;
        }
        let push = |start: usize, end: usize, out: &mut Matches| {
            for matcher in &self.matchers {
                matcher.push_if_match(chunk, start, end, out);
            }
        };

        let Some(buffer) = &self.buffer else {
            let mut start = 0;
            while start < chunk.len() {
                let end = memchr::memchr(b'\n', &chunk[start..]).map_or(chunk.len(), |i| start + i);
                push(start, end, out);
                start = end + 1;
            }
            return;
        };

        let next = |pos: usize| match &self.literal {
            Some(literal) => literal.find(chunk, pos),
            None => buffer.find_at(chunk, pos).map(|m| m.start()),
        };

        let mut pos = 0;
        while pos < chunk.len() {
            let Some(at) = next(pos) else {
                break;
            };
            let start = memchr::memrchr(b'\n', &chunk[pos..at]).map_or(pos, |i| pos + i + 1);
            if start == chunk.len() {
                break;
            }
            let end = memchr::memchr(b'\n', &chunk[at..]).map_or(chunk.len(), |i| at + i);
            // Past the line's end only its terminator matters to `buffer`, and the end of the
            // haystack looks the same to it.
            if self.literal.as_ref().is_none_or(FoldedLiteral::is_whole)
                || buffer.find_at(&chunk[..end], start).is_some()
            {
                push(start, end, out);
            }
            pos = end + 1;
        }
    }
}

fn exclude_newline(hir: Hir) -> Option<Hir> {
    Some(match hir.into_kind() {
        HirKind::Empty => Hir::empty(),
        HirKind::Literal(Literal(lit)) => {
            if lit.contains(&b'\n') {
                return None;
            }
            Hir::literal(lit)
        }
        HirKind::Class(Class::Unicode(mut c)) => {
            c.difference(&ClassUnicode::new([ClassUnicodeRange::new('\n', '\n')]));
            Hir::class(Class::Unicode(c))
        }
        HirKind::Class(Class::Bytes(mut c)) => {
            c.difference(&ClassBytes::new([ClassBytesRange::new(b'\n', b'\n')]));
            Hir::class(Class::Bytes(c))
        }
        // A line's end is before "\r\n" as well as "\n"; matching `\r` elsewhere is left to `.`.
        HirKind::Look(Look::StartLF) => Hir::look(Look::StartCRLF),
        HirKind::Look(Look::EndLF) => Hir::look(Look::EndCRLF),
        HirKind::Look(look) => Hir::look(look),
        HirKind::Repetition(mut rep) => {
            rep.sub = Box::new(exclude_newline(*rep.sub)?);
            Hir::repetition(rep)
        }
        HirKind::Capture(mut cap) => {
            cap.sub = Box::new(exclude_newline(*cap.sub)?);
            Hir::capture(cap)
        }
        HirKind::Concat(subs) => Hir::concat(
            subs.into_iter()
                .map(exclude_newline)
                .collect::<Option<_>>()?,
        ),
        HirKind::Alternation(subs) => Hir::alternation(
            subs.into_iter()
                .map(exclude_newline)
                .collect::<Option<_>>()?,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::{Matcher, Patterns};
    use crate::query::Matches;

    fn search(patterns: &Patterns, chunk: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Matches::default();
        patterns.search(chunk, &mut out);
        out.sort();
        out.lines
            .iter()
            .map(|range| out.line(range).to_vec())
            .collect()
    }

    #[test]
    fn buffer_search_matches_line_search() {
        let chunk =
            "/Users/a/SRC/Main.rs\n/x/a\rb/src\r\n\n/opt/Src/lib/ß/STRASSE\r\n/y/ſrc/\u{212a}ey\n\
                     /z/src src SRC\n/w/M\rn.RS\n/É/école/ÉCOLE\n/srcsrc/Src\n/end/ReadMe.md\r"
                .as_bytes();
        let cases: &[&[&str]] = &[
            &["src"],
            &["SRC"],
            &["key"],
            &["main.rs"],
            &["a.b"],
            &["école"],
            &["(?-i)src"],
            &["(?-i:[A-Z])rc"],
            &["\\p{Lu}rc"],
            &["\\bsrc\\b"],
            &["src$"],
            &["^/z"],
            &["readme", "SRC"],
            &["src", "src"],
            &["x*", "src"],
            &["\\Asrc", "main"],
            &["key", "(?-i)Src"],
        ];
        for patterns in cases {
            for ignore_case in [false, true] {
                let matchers: Vec<_> = patterns
                    .iter()
                    .map(|p| Matcher::new(p, ignore_case))
                    .collect();
                let line_only = Patterns {
                    matchers: matchers.clone(),
                    buffer: None,
                    literal: None,
                };
                assert_eq!(
                    search(&Patterns::new(matchers), chunk),
                    search(&line_only, chunk),
                    "{patterns:?} ignore_case={ignore_case}"
                );
            }
        }
    }
}
