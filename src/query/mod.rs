mod matcher;
mod prefilter;

use std::{
    cmp::Reverse,
    collections::BinaryHeap,
    io::{self, Write},
    ops::Range,
    process, thread,
};

use anyhow::Result;
use colored::Colorize;

use crate::{db::Database, frame::Consume};
pub use matcher::{Matcher, Patterns};

const OUTPUT_BUFFER: usize = 256 * 1024;
const MERGE_PART_MIN: usize = 16 * 1024;
const MERGE_SAMPLE_STEP: usize = 64;

impl Database {
    pub fn query(&self, patterns: &Patterns) -> Result<()> {
        if !self.data.parent().expect("Unable to get parent").exists() {
            print_err!(
                "Database {} doesn't exist. Perhaps you forgot to run lolcate --create {} ?",
                &self.name.green(),
                &self.name.green()
            );
            process::exit(1);
        }

        if !self.data.exists() {
            print_err!(
                "Database {} is empty. Perhaps you forgot to run lolcate --update {} ?",
                &self.name.green(),
                &self.name.green()
            );
            process::exit(1);
        }

        let mut writer = io::BufWriter::with_capacity(OUTPUT_BUFFER, io::stdout().lock());

        let runs: Vec<_> = self
            .scan(|| Search {
                // Each thread gets its own regex caches; a shared regex's cache pool is contended
                // when every candidate line takes a search call.
                patterns: patterns.clone(),
                out: Matches::default(),
            })?
            .into_iter()
            .map(|search| search.out)
            .collect();

        let runs: Vec<Vec<&[u8]>> = runs
            .iter()
            .map(|run| run.lines.iter().map(|range| run.line(range)).collect())
            .collect();
        write_merged(&runs, &mut writer);

        Ok(())
    }
}

/// Writes the merge of the sorted `runs`, one line each, merging disjoint ranges of lines in
/// parallel.
fn write_merged(runs: &[Vec<&[u8]>], writer: &mut impl Write) {
    let total: usize = runs.iter().map(Vec::len).sum();
    let parts = (total / MERGE_PART_MIN).clamp(1, num_cpus::get());
    if parts == 1 {
        let _ = writer.write_all(&merge(runs.iter().map(Vec::as_slice).collect()));
        return;
    }

    let mut samples: Vec<&[u8]> = runs
        .iter()
        .flat_map(|run| run.iter().step_by(MERGE_SAMPLE_STEP).copied())
        .collect();
    samples.sort_unstable();
    let splitters: Vec<&[u8]> = (1..parts)
        .map(|p| samples[p * samples.len() / parts])
        .collect();
    // bounds[r][p]..bounds[r][p + 1] is the part of run r that goes into part p.
    let bounds: Vec<Vec<usize>> = runs
        .iter()
        .map(|run| {
            let inner = splitters
                .iter()
                .map(|splitter| run.partition_point(|line| line < splitter));
            std::iter::once(0).chain(inner).chain([run.len()]).collect()
        })
        .collect();

    thread::scope(|s| {
        let merged: Vec<_> = (0..parts)
            .map(|p| {
                let bounds = &bounds;
                s.spawn(move || {
                    merge(
                        runs.iter()
                            .zip(bounds)
                            .map(|(run, b)| &run[b[p]..b[p + 1]])
                            .collect(),
                    )
                })
            })
            .collect();
        for part in merged {
            let _ = writer.write_all(&part.join().expect("Unable to join threads"));
        }
    });
}

fn merge(runs: Vec<&[&[u8]]>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut cursors: Vec<_> = runs.into_iter().map(<[&[u8]]>::iter).collect();
    let mut heap: BinaryHeap<_> = cursors
        .iter_mut()
        .enumerate()
        .filter_map(|(i, cursor)| Some(Reverse((*cursor.next()?, i))))
        .collect();

    while let Some(Reverse((line, i))) = heap.pop() {
        out.extend_from_slice(line);
        out.push(b'\n');
        if let Some(next) = cursors[i].next() {
            heap.push(Reverse((*next, i)));
        }
    }
    out
}

/// Collects one thread's matches, sorted.
struct Search {
    patterns: Patterns,
    out: Matches,
}

impl Consume for Search {
    fn consume(&mut self, lines: &[u8]) {
        self.patterns.search(lines, &mut self.out);
    }

    fn finish(&mut self) {
        self.out.sort();
    }
}

/// Highlighted matching lines, stored back to back rather than allocated one by one.
#[derive(Default)]
pub struct Matches {
    bytes: Vec<u8>,
    lines: Vec<Range<usize>>,
}

impl Matches {
    fn line(&self, range: &Range<usize>) -> &[u8] {
        &self.bytes[range.clone()]
    }

    fn sort(&mut self) {
        let bytes = &self.bytes;
        self.lines
            .sort_unstable_by(|a, b| bytes[a.clone()].cmp(&bytes[b.clone()]));
    }
}
