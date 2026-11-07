use regex_syntax::hir::{Class, Hir, HirKind, Literal};

const MIN_PARTIAL: usize = 3;

/// A literal, ASCII letters optionally matched in either case, that every match of a pattern
/// contains. Found with a NEON scan, it locates candidate lines for a case-insensitive pattern
/// several times faster than the regex engine's own prefilter.
#[derive(Clone, Debug)]
pub struct FoldedLiteral {
    /// The literal, with case-insensitive letters lowercased.
    needle: Vec<u8>,
    /// `0x20` where `needle` matches a letter in either case, else 0; `(byte | mask) == needle`.
    mask: Vec<u8>,
    index1: usize,
    index2: usize,
    /// Whether the literal is the whole pattern, so every occurrence is a match.
    whole: bool,
}

/// One byte of a literal run: the byte, lowercased if it matches either case.
fn folded_byte(hir: &Hir) -> Option<Vec<(u8, u8)>> {
    match hir.kind() {
        HirKind::Literal(Literal(bytes)) => Some(bytes.iter().map(|&b| (b, 0)).collect()),
        HirKind::Class(Class::Unicode(class)) => match class.ranges() {
            [a, b] if a.start() == a.end() && b.start() == b.end() => {
                ascii_case_pair(u32::from(a.start()), u32::from(b.start()))
            }
            _ => None,
        },
        HirKind::Class(Class::Bytes(class)) => match class.ranges() {
            [a, b] if a.start() == a.end() && b.start() == b.end() => {
                ascii_case_pair(u32::from(a.start()), u32::from(b.start()))
            }
            _ => None,
        },
        _ => None,
    }
}

/// `[(lowercase, 0x20)]` if `upper`/`lower` are exactly an ASCII letter's two cases. A letter
/// whose class holds more (`k` with the Kelvin sign, `s` with the long s) isn't a pair.
fn ascii_case_pair(upper: u32, lower: u32) -> Option<Vec<(u8, u8)>> {
    let (upper, lower) = (u8::try_from(upper).ok()?, u8::try_from(lower).ok()?);
    (upper.is_ascii_uppercase() && lower == upper.to_ascii_lowercase()).then(|| vec![(lower, 0x20)])
}

impl FoldedLiteral {
    /// The longest literal run among `hir`'s top-level concatenation, if it matches at least
    /// one letter in either case; a case-sensitive literal is already found fast by the regex
    /// engine. Every element of a concatenation is part of every match, in order and contiguous,
    /// so the run is in every match. A run that is only part of the pattern must be at least
    /// `MIN_PARTIAL` bytes, or its occurrences outside matches make the scan a net loss.
    pub fn from_hir(hir: &Hir) -> Option<Self> {
        if !cfg!(target_arch = "aarch64") {
            return None;
        }
        let HirKind::Concat(subs) = hir.kind() else {
            return None;
        };

        let mut best: Vec<(u8, u8)> = Vec::new();
        let mut run: Vec<(u8, u8)> = Vec::new();
        let mut breaks = false;
        for sub in subs {
            if let Some(bytes) = folded_byte(sub) {
                run.extend(bytes);
            } else {
                breaks = true;
                if run.len() > best.len() {
                    best = std::mem::take(&mut run);
                }
                run.clear();
            }
        }
        if run.len() > best.len() {
            best = run;
        }

        let whole = !breaks;
        if best.len() < if whole { 2 } else { MIN_PARTIAL }
            || best.iter().all(|&(_, mask)| mask == 0)
        {
            return None;
        }
        let (needle, mask): (Vec<u8>, Vec<u8>) = best.into_iter().unzip();
        let pair = memchr::arch::all::packedpair::Pair::new(&needle)?;
        Some(Self {
            needle,
            mask,
            index1: usize::from(pair.index1()),
            index2: usize::from(pair.index2()),
            whole,
        })
    }

    #[must_use]
    pub fn is_whole(&self) -> bool {
        self.whole
    }

    fn matches_at(&self, haystack: &[u8], at: usize) -> bool {
        haystack[at..at + self.needle.len()]
            .iter()
            .zip(&self.needle)
            .zip(&self.mask)
            .all(|((&b, &n), &m)| b | m == n)
    }

    /// The first position at or after `start` where the literal occurs.
    pub fn find(&self, haystack: &[u8], start: usize) -> Option<usize> {
        let last = haystack.len().checked_sub(self.needle.len())?;
        #[cfg(target_arch = "aarch64")]
        // SAFETY: NEON is part of the aarch64 baseline.
        let start = match unsafe { self.find_neon(haystack, start, last) } {
            Ok(found) => return Some(found),
            Err(rest) => rest,
        };
        (start..=last).find(|&at| self.matches_at(haystack, at))
    }

    /// Scans 16 candidate positions at a time from `at`; on a miss, returns where the scan stopped,
    /// fewer than 16 positions before `last`.
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    fn find_neon(&self, haystack: &[u8], mut at: usize, last: usize) -> Result<usize, usize> {
        use std::arch::aarch64::{
            vandq_u8, vceqq_u8, vdupq_n_u8, vget_lane_u64, vld1q_u8, vorrq_u8, vreinterpret_u64_u8,
            vreinterpretq_u16_u8, vshrn_n_u16,
        };

        let (i1, i2) = (self.index1, self.index2);
        let needle1 = vdupq_n_u8(self.needle[i1]);
        let mask1 = vdupq_n_u8(self.mask[i1]);
        let needle2 = vdupq_n_u8(self.needle[i2]);
        let mask2 = vdupq_n_u8(self.mask[i2]);
        while at + 15 <= last {
            // SAFETY: `at + 15 + i < at + 15 + needle.len() <= haystack.len()` for both indices,
            // so each 16-byte load is in bounds.
            let (bytes1, bytes2) = unsafe {
                (
                    vld1q_u8(haystack.as_ptr().add(at + i1)),
                    vld1q_u8(haystack.as_ptr().add(at + i2)),
                )
            };
            let eq = vandq_u8(
                vceqq_u8(vorrq_u8(bytes1, mask1), needle1),
                vceqq_u8(vorrq_u8(bytes2, mask2), needle2),
            );
            // Four bits per lane, set where both bytes of the pair match.
            let mut lanes = vget_lane_u64::<0>(vreinterpret_u64_u8(vshrn_n_u16::<4>(
                vreinterpretq_u16_u8(eq),
            )));
            while lanes != 0 {
                let lane = (lanes.trailing_zeros() / 4) as usize;
                if self.matches_at(haystack, at + lane) {
                    return Ok(at + lane);
                }
                lanes &= !(0xF << (lane * 4));
            }
            at += 16;
        }
        Err(at)
    }
}

#[cfg(all(test, target_arch = "aarch64"))]
mod tests {
    use super::FoldedLiteral;

    fn literal(pattern: &str) -> Option<FoldedLiteral> {
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(true)
            .build()
            .parse(pattern)
            .expect("valid pattern");
        FoldedLiteral::from_hir(&hir)
    }

    #[test]
    fn extracts_the_longest_case_pair_run() {
        let needle = |p: &str| literal(p).map(|l| String::from_utf8(l.needle).expect("ASCII"));
        assert_eq!(needle("main.rs").as_deref(), Some("main"));
        assert_eq!(needle("node_modules").as_deref(), Some("node_module"));
        assert!(literal("key").is_none());
        assert!(literal("main").is_some_and(|l| l.is_whole()));
        assert!(literal("main.rs").is_some_and(|l| !l.is_whole()));
        assert_eq!(needle("(?-i:Src)lib").as_deref(), Some("Srclib"));
        assert!(literal("(?-i)src").is_none());
        assert!(literal("a").is_none());
        assert!(literal("a.b").is_none());
        assert!(literal("src|lib").is_none());
        assert!(literal("ß").is_none());
    }

    #[test]
    fn find_matches_a_naive_scan() {
        let mut seed = 0x2545_F491_u32;
        let mut rand = move |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed as usize % n
        };
        let alphabet = b"abcdeMAINmainReAdMe/._-\n\r09@[`{";
        for pattern in ["main", "readme", "a-b", "e-m", "(?-i:M)ain", "zz", "abc."] {
            let lit = literal(pattern).expect("literal");
            for len in [0, 1, 3, 15, 16, 17, 31, 32, 33, 100, 1000] {
                let haystack: Vec<u8> = (0..len).map(|_| alphabet[rand(alphabet.len())]).collect();
                let naive = |start: usize| {
                    (start..=haystack.len().saturating_sub(lit.needle.len())).find(|&at| {
                        at + lit.needle.len() <= haystack.len() && lit.matches_at(&haystack, at)
                    })
                };
                for start in [0, 1, 7, len / 2] {
                    assert_eq!(
                        lit.find(&haystack, start),
                        naive(start),
                        "{pattern} len={len} start={start}"
                    );
                }
            }
        }
    }
}
