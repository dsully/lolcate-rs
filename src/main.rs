#![deny(clippy::all, clippy::pedantic, clippy::unwrap_used)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashSet},
    io::{self, Read, Write},
    ops::Range,
    path::{Path, PathBuf},
    process, str,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

mod frame;
use anyhow::{Context, Result};
use bstr::io::BufReadExt;
use clap::Parser;
use colored::Colorize;
use crossbeam_channel as channel;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use file_locking::FileExt as _;
use frame::{FrameReader, Xxh32};
use fs_err as fs;
use fs4::FileExt;
use home_dir::HomeDirExt;
use indicatif::{ProgressBar, ProgressStyle};
use lz4::EncoderBuilder;
use num_format::{Locale, ToFormattedString};
use regex::{Regex, RegexBuilder, bytes};
use regex_syntax::hir::{
    Class, ClassBytes, ClassBytesRange, ClassUnicode, ClassUnicodeRange, Hir, HirKind, Literal,
    Look,
};
use serde::Deserialize;

static UPPER_RE: OnceLock<Regex> = OnceLock::new();

const CHUNK_SIZE: usize = 1 << 21;
const CHUNK_QUEUE: usize = 16;
const OUTPUT_BUFFER: usize = 256 * 1024;

static PROJECT_CONFIG_TEMPLATE: &str = r#"
description = ""

# Paths to index.
paths = [
  # "~/first/dir",
  # "/second/dir"
]

"#;

static PROJECT_IGNORE_TEMPLATE: &str = r"# Paths / files to ignore.
# Use the same syntax as gitignore(5).
# Common patterns:
#
# .git
# *~
# target/
";

macro_rules! print_err {
    ($($err:tt)*) => ({
        eprintln!("{}: {}", "Error".red().bold(), format!($($err)*));
    })
}

#[derive(Parser)]
#[command(name = "lolcate")]
#[command(version = "0.2")]
#[allow(clippy::struct_excessive_bools)]
pub struct Args {
    /// Create a database
    #[arg(long, conflicts_with_all = &["pattern", "update", "info"])]
    pub create: bool,

    /// Display configuration information and existing databases
    #[arg(long, conflicts_with_all = &["pattern", "update", "create", "database"])]
    pub info: bool,

    /// Update database
    #[arg(short, long, conflicts_with_all = &["pattern", "create", "info"])]
    pub update: bool,

    /// Database to be used / created
    #[arg(long = "db", default_value = "default")]
    pub database: String,

    /// Query / update all databases
    #[arg(long, conflicts_with_all = &["create", "info"])]
    pub all: bool,

    /// Search case-insensitively [default: smart-case]
    #[arg(short, long, conflicts_with_all = &["create", "info", "update"])]
    pub ignore_case: bool,

    /// Match only basename against PATTERN
    #[arg(short, long = "basename", conflicts_with_all = &["create", "info", "update"])]
    pub basename_pattern: Option<Vec<String>>,

    /// PATTERN
    #[clap(required = false)]
    pub pattern: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LocalConfig {
    pub description: String,
    pub paths: Vec<PathBuf>,
}

#[derive(Clone, Copy)]
pub enum PathType {
    Config,
    Data,
}

pub enum WorkerResult {
    Entry(PathBuf),
    Error(ignore::Error),
}

pub enum DirEntry {
    Normal(ignore::DirEntry),
    BrokenSymlink(PathBuf),
}

impl DirEntry {
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            DirEntry::Normal(e) => e.path(),
            DirEntry::BrokenSymlink(pathbuf) => pathbuf.as_path(),
        }
    }

    #[must_use]
    pub fn file_type(&self) -> Option<std::fs::FileType> {
        match self {
            DirEntry::Normal(e) => e.file_type(),
            DirEntry::BrokenSymlink(pathbuf) => {
                pathbuf.symlink_metadata().map(|m| m.file_type()).ok()
            }
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Database {
    pub name: String,
    pub description: String,
    pub root: PathBuf,
    pub config: PathBuf,
    pub ignores: PathBuf,
    pub data: PathBuf,
}

impl Database {
    pub fn new(name: &str) -> Result<Self> {
        let config = Self::xdg_path(PathType::Config)?;
        let data = Self::xdg_path(PathType::Data)?;

        let config_path = config.join(name).join("config.toml");
        let database_path = data.join(name).join("db.lz4");

        let description = Self::config(name).unwrap_or_default().description;

        Ok(Self {
            name: name.to_string(),
            description,
            root: config.join(name),
            config: config_path,
            ignores: config.join(name).join("ignores"),
            data: database_path,
        })
    }

    pub fn all() -> Result<Vec<Database>> {
        let config = Self::xdg_path(PathType::Config)?;

        walkdir::WalkDir::new(config)
            .min_depth(1)
            .into_iter()
            .filter_map(std::result::Result::ok)
            .filter(|e| e.file_type().is_dir())
            .map(|e| {
                Database::new(
                    e.path()
                        .file_name()
                        .expect("Unable to get file name")
                        .to_string_lossy()
                        .to_string()
                        .as_str(),
                )
            })
            .collect()
    }

    pub fn files(&self) -> Result<usize> {
        if !self.data.exists() {
            return Ok(0);
        }
        let reader = io::BufReader::new(lz4::Decoder::new(fs::File::open(&self.data)?)?);
        Ok(reader.byte_lines().count())
    }

    fn xdg_path(path_type: PathType) -> Result<PathBuf> {
        let xdg_dir = xdg::BaseDirectories::with_prefix("lolcate");

        Ok(match path_type {
            PathType::Config => xdg_dir
                .get_config_home()
                .context("Unable to get config home")?,
            PathType::Data => xdg_dir.get_data_home().context("Unable to get data home")?,
        })
    }

    pub fn config(name: &str) -> Result<LocalConfig> {
        let filename = Self::xdg_path(PathType::Config)?
            .join(name)
            .join("config.toml");

        Ok(Figment::from(Toml::file(filename)).extract()?)
    }

    pub fn create(&self) -> Result<()> {
        if self.root.exists() {
            print_err!("database {} already exists", &self.name.green());
            process::exit(1);
        }

        fs::create_dir_all(&self.root)?;

        let mut f = fs::File::create(&self.config)?;
        f.write_all(PROJECT_CONFIG_TEMPLATE.as_bytes())?;

        f = fs::File::create(&self.ignores)?;
        f.write_all(PROJECT_IGNORE_TEMPLATE.as_bytes())?;

        println!("Created database {}.", self.name.green().bold());
        println!("Please edit:\n");
        println!("- the configuration file: {}", self.config.display());
        println!("- the ignores file:       {}", self.ignores.display());

        process::exit(0);
    }

    pub fn info() -> Result<()> {
        let databases = Self::all()?;

        if databases.is_empty() {
            println!("{}", "No databases found".cyan());
        } else {
            println!("{}", "Databases:".cyan());

            for db in databases {
                println!("  {}", db.name.green());
                println!("    Description: {}", db.description);
                println!("         Config: {}", db.config.display());
                println!("        Ignores: {}", db.ignores.display());
                println!("  Database File: {}", db.data.display());
                println!(
                    "  Paths Indexed: {}",
                    db.files()?.to_formatted_string(&Locale::en)
                );
                println!(
                    "   Last Updated: {}",
                    humantime::format_rfc3339(db.data.metadata()?.modified()?)
                );
            }
        }

        println!();
        Ok(())
    }

    pub fn paths(&self) -> Result<Vec<PathBuf>> {
        let paths = Self::config(&self.name)?
            .paths
            .iter()
            .filter_map(|p| p.expand_home().ok())
            .collect::<Vec<PathBuf>>();
        Ok(paths)
    }

    pub fn query(&self, patterns: &[Matcher]) -> Result<()> {
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

        let mut reader = FrameReader::new(fs::File::open(&self.data)?)?;
        let mut writer = io::BufWriter::with_capacity(OUTPUT_BUFFER, io::stdout().lock());

        let (tx, rx) = channel::bounded::<Arc<Vec<u8>>>(CHUNK_QUEUE);
        let (hash_tx, hash_rx) = channel::bounded::<Arc<Vec<u8>>>(CHUNK_QUEUE);

        let runs = thread::scope(|s| -> Result<Vec<Matches>> {
            let decoder = s.spawn(move || {
                read_chunks(&mut reader, &[tx, hash_tx])?;
                reader.stored_checksum()
            });
            let hasher = s.spawn(move || {
                let mut hash = Xxh32::default();
                for chunk in hash_rx {
                    hash.update(&chunk);
                }
                hash.digest()
            });

            let workers: Vec<_> = (0..num_cpus::get())
                .map(|_| {
                    let rx = rx.clone();
                    s.spawn(move || {
                        let mut out = Matches::default();
                        for chunk in rx {
                            for matcher in patterns {
                                matcher.search(&chunk, &mut out);
                            }
                        }
                        out.sort();
                        out
                    })
                })
                .collect();
            drop(rx);

            let runs: Vec<_> = workers
                .into_iter()
                .map(|worker| worker.join().expect("Unable to join threads"))
                .collect();
            let stored = decoder.join().expect("Unable to join threads")?;
            let actual = hasher.join().expect("Unable to join threads");
            if stored.is_some_and(|stored| stored != actual) {
                return Err(io::Error::other("LZ4 error: ERROR_contentChecksum_invalid").into());
            }
            Ok(runs)
        })?;

        let mut cursors: Vec<_> = runs.iter().map(|run| run.lines.iter()).collect();
        let mut heap: BinaryHeap<_> = cursors
            .iter_mut()
            .enumerate()
            .filter_map(|(i, cursor)| Some(Reverse((runs[i].line(cursor.next()?), i))))
            .collect();

        while let Some(Reverse((line, i))) = heap.pop() {
            let _ = writer.write_all(line);
            let _ = writer.write_all(b"\n");
            if let Some(next) = cursors[i].next() {
                heap.push(Reverse((runs[i].line(next), i)));
            }
        }

        Ok(())
    }

    fn walker(
        &self,
        filter: impl Fn(&ignore::DirEntry) -> bool + Send + Sync + 'static,
    ) -> Result<ignore::WalkParallel> {
        let mut paths = self.paths()?.into_iter();

        let mut wd = ignore::WalkBuilder::new(paths.next().expect("No paths provided"));

        // ignore >= 0.4.24 anchors patterns in the ignores file and the global gitignore at the CWD;
        // anchor them at the filesystem root so absolute patterns keep working wherever lolcate runs.
        wd.current_dir("/")
            .hidden(false)
            .follow_links(false)
            .git_global(true);

        for path in paths.filter(|path| path.exists()) {
            wd.add(path);
        }

        wd.add_ignore(&self.ignores);
        wd.threads(num_cpus::get());
        wd.filter_entry(filter);

        Ok(wd.build_parallel())
    }

    #[allow(clippy::too_many_lines)]
    pub fn update(&self) -> Result<()> {
        //
        if !self.config.exists() {
            print_err!(
                "Config file not found for database {}.\n Perhaps you forgot to run lolcate --create \
             {} ?",
                &self.name.green(),
                &self.name.green()
            );
            process::exit(1);
        }

        if self.paths().unwrap_or_default().is_empty() {
            print_err!(
                "{} needs at least one directory to scan",
                self.config.display().to_string().green()
            );
            process::exit(1);
        }

        let parent = self.data.parent().expect("Unable to get parent");

        fs::create_dir_all(parent)?;

        let lockfile = parent.join("lock");
        let lock = std::fs::File::create(&lockfile)?;

        println!("Waiting for '{}' database lock ...", self.name.green());

        let _ = lock.lock_exclusive()?;

        println!("File lock for '{}' acquired!", self.name.green());

        let database = fs::File::create(&self.data)?;

        let (tx, rx) = channel::bounded::<WorkerResult>(8 * 1024);

        let s = spinner()?;
        let start_time = SystemTime::now();

        s.set_message(format!("{} {}...", "Updating".green().bold(), self.name));

        let stdout_thread = thread::spawn(move || {
            let mut encoder = EncoderBuilder::new()
                .level(3)
                .block_mode(lz4::BlockMode::Linked)
                .block_size(lz4::BlockSize::Max256KB)
                .build(database)?;

            for entry in rx {
                match entry {
                    WorkerResult::Entry(value) => {
                        writeln!(encoder, "{}", value.display())?;
                    }
                    WorkerResult::Error(err) => print_err!("{}", err.to_string()),
                }
            }
            let (_output, result) = encoder.finish();
            result
        });

        let indexer = Arc::new(Indexer {
            tx,
            unsearchable: Mutex::new(HashSet::new()),
            any_unsearchable: AtomicBool::new(false),
        });

        // Excluded and bundle directories are decided here rather than in the visitor, because the
        // walker reads a directory (and its ignore files) before visiting it.
        let filter = {
            let indexer = indexer.clone();
            move |e: &ignore::DirEntry| {
                if !e.file_type().is_none_or(|t| t.is_dir() || t.is_symlink()) {
                    return true;
                }
                matches!(
                    indexer.visit(e.path(), e.file_type(), false),
                    ignore::WalkState::Continue
                )
            }
        };

        self.walker(filter)?.run(|| {
            let indexer = indexer.clone();

            Box::new(move |e| {
                // Taken from sharkdp/fd
                let entry = match e {
                    Ok(e) => DirEntry::Normal(e),
                    Err(ignore::Error::WithPath { path, err: error }) => match error.as_ref() {
                        ignore::Error::Io(io_err)
                            if io_err.kind() == io::ErrorKind::NotFound
                                && path
                                    .symlink_metadata()
                                    .ok()
                                    .is_some_and(|m| m.file_type().is_symlink()) =>
                        {
                            DirEntry::BrokenSymlink(path)
                        }
                        _ => {
                            return indexer.send(WorkerResult::Error(ignore::Error::WithPath {
                                path,
                                err: error,
                            }));
                        }
                    },
                    Err(err) => return indexer.send(WorkerResult::Error(err)),
                };

                let root = match &entry {
                    DirEntry::Normal(e) => e.depth() == 0,
                    DirEntry::BrokenSymlink(_) => true,
                };
                let file_type = entry.file_type();
                if !root && file_type.is_none_or(|t| t.is_dir() || t.is_symlink()) {
                    // Already indexed by the filter.
                    return ignore::WalkState::Continue;
                }
                indexer.visit(entry.path(), file_type, root)
            })
        });

        // The writer finishes once the last sender, held by the indexer, is gone.
        drop(indexer);

        stdout_thread.join().expect("Unable to join threads")?;

        s.finish_with_message(format!(
            "{} {} in {}",
            "Updated".green().bold(),
            self.name,
            humantime::format_duration(SystemTime::now().duration_since(start_time)?),
        ));

        FileExt::unlock(&lock)?;

        if lockfile.exists() {
            fs::remove_file(lockfile)?;
        }

        Ok(())
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

/// Sends `reader`'s contents in chunks of roughly `CHUNK_SIZE` that each end at a line boundary.
fn read_chunks(
    reader: &mut impl io::Read,
    txs: &[channel::Sender<Arc<Vec<u8>>>],
) -> io::Result<()> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut len = 0;

    loop {
        if len == buf.len() {
            buf.resize(buf.len() * 2, 0);
        }
        let n = read_full(reader, &mut buf[len..])?;
        let eof = len + n < buf.len();
        len += n;

        let end = if eof {
            len
        } else if let Some(i) = memchr::memrchr(b'\n', &buf[..len]) {
            i + 1
        } else {
            continue;
        };

        let mut next = vec![0u8; buf.len()];
        next[..len - end].copy_from_slice(&buf[end..len]);
        buf.truncate(end);
        let chunk = Arc::new(std::mem::replace(&mut buf, next));
        len -= end;

        if txs.iter().any(|tx| tx.send(chunk.clone()).is_err()) || eof {
            return Ok(());
        }
    }
}

/// Decides which walked paths are indexed and sends them to the database writer.
struct Indexer {
    tx: channel::Sender<WorkerResult>,
    unsearchable: Mutex<HashSet<PathBuf>>,
    any_unsearchable: AtomicBool,
}

impl Indexer {
    fn send(&self, result: WorkerResult) -> ignore::WalkState {
        match self.tx.send(result) {
            Ok(()) => ignore::WalkState::Continue,
            Err(_) => ignore::WalkState::Quit,
        }
    }

    /// Indexes `path` unless it is excluded; the result says whether to descend into it.
    fn visit(
        &self,
        path: &Path,
        file_type: Option<std::fs::FileType>,
        root: bool,
    ) -> ignore::WalkState {
        // Bundle directories are never descended into, so only a root can lie inside one.
        if root && is_inside_bundle(path) {
            return ignore::WalkState::Skip;
        }

        let is_dir = file_type.is_some_and(|t| t.is_dir());
        if file_type.is_none_or(|t| t.is_dir() || t.is_symlink()) {
            match probe_dir(path) {
                DirProbe::Tagged => return ignore::WalkState::Skip,
                DirProbe::Unsearchable if is_dir => {
                    self.unsearchable
                        .lock()
                        .expect("Unable to lock")
                        .insert(path.to_owned());
                    self.any_unsearchable.store(true, Ordering::Release);
                }
                _ => {}
            }
        }

        let state = if is_dir && is_bundle_dir(path) {
            ignore::WalkState::Skip
        } else {
            ignore::WalkState::Continue
        };

        // Without search permission on its directory an entry is listed but can't be stat'ed;
        // such entries are not indexed.
        if self.any_unsearchable.load(Ordering::Acquire)
            && path.parent().is_some_and(|parent| {
                self.unsearchable
                    .lock()
                    .expect("Unable to lock")
                    .contains(parent)
            })
        {
            return state;
        }

        match self.send(WorkerResult::Entry(path.to_owned())) {
            ignore::WalkState::Quit => ignore::WalkState::Quit,
            _ => state,
        }
    }
}

fn is_inside_bundle(path: &Path) -> bool {
    path.to_str().is_some_and(|path| {
        [".app/", ".bundle/", ".fcpbundle/", ".framework/"]
            .iter()
            .any(|bundle| path.contains(bundle))
    })
}

fn is_bundle_dir(path: &Path) -> bool {
    path.to_str().is_some_and(|path| {
        [".app", ".bundle", ".fcpbundle", ".framework"]
            .iter()
            .any(|suffix| path.ends_with(suffix))
    })
}
enum DirProbe {
    Tagged,
    Untagged,
    Unsearchable,
}

/// One `open` both checks `dir` for a valid `CACHEDIR.TAG` and, since that needs search
/// permission on `dir`, whether its entries can be stat'ed.
fn probe_dir(dir: &Path) -> DirProbe {
    match std::fs::File::open(dir.join("CACHEDIR.TAG")) {
        Ok(mut tag) => {
            let mut header = [0u8; cachedir::HEADER.len()];
            if tag
                .read(&mut header)
                .is_ok_and(|n| n == header.len() && header == *cachedir::HEADER)
            {
                DirProbe::Tagged
            } else {
                DirProbe::Untagged
            }
        }
        // EACCES may also come from an unreadable tag file in a searchable directory.
        Err(e)
            if e.kind() == io::ErrorKind::PermissionDenied
                && dir.join(".").symlink_metadata().is_err() =>
        {
            DirProbe::Unsearchable
        }
        Err(_) => DirProbe::Untagged,
    }
}
fn read_full(reader: &mut impl io::Read, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

pub struct Matcher {
    line: Regex,
    /// Finds a superset of the lines `line` matches when run over a whole newline-separated buffer.
    buffer: Option<bytes::Regex>,
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
        let re =
            UPPER_RE.get_or_init(|| Regex::new(r"[[:upper:]]").expect("Unable to compile regex"));
        let case_insensitive = ignore_case || !re.is_match(pattern);

        let line = match RegexBuilder::new(pattern)
            .case_insensitive(case_insensitive)
            .build()
        {
            Ok(re) => re,
            Err(e) => panic!("invalid regex: {e}"),
        };

        Self {
            line,
            buffer: Self::buffer_regex(buffer_pattern, case_insensitive),
        }
    }

    // Parsed in multi-line + CRLF mode with `\n` removed from every class, so a buffer match never
    // spans lines and any per-line match is a buffer match at the same offset. `\A`/`\z` have no
    // such equivalent, so those patterns use line mode.
    fn buffer_regex(pattern: &str, case_insensitive: bool) -> Option<bytes::Regex> {
        let hir = regex_syntax::ParserBuilder::new()
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .crlf(true)
            .build()
            .parse(pattern)
            .ok()?;
        let looks = hir.properties().look_set();
        if looks.contains(Look::Start) || looks.contains(Look::End) {
            return None;
        }
        bytes::Regex::new(&exclude_newline(hir)?.to_string()).ok()
    }

    /// Pushes every line of `chunk` matching this pattern, highlighted, onto `out`.
    pub fn search(&self, chunk: &[u8], out: &mut Matches) {
        let Some(buffer) = &self.buffer else {
            let mut start = 0;
            while start < chunk.len() {
                let end = memchr::memchr(b'\n', &chunk[start..]).map_or(chunk.len(), |i| start + i);
                self.push_if_match(chunk, start, end, out);
                start = end + 1;
            }
            return;
        };

        let mut pos = 0;
        while pos < chunk.len() {
            let Some(m) = buffer.find_at(chunk, pos) else {
                break;
            };
            let start = memchr::memrchr(b'\n', &chunk[pos..m.start()]).map_or(pos, |i| pos + i + 1);
            if start == chunk.len() {
                break;
            }
            let end =
                memchr::memchr(b'\n', &chunk[m.start()..]).map_or(chunk.len(), |i| m.start() + i);
            self.push_if_match(chunk, start, end, out);
            pos = end + 1;
        }
    }

    fn push_if_match(&self, chunk: &[u8], start: usize, end: usize, out: &mut Matches) {
        let mut line = &chunk[start..end];
        if end < chunk.len() && line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        let line = str::from_utf8(line).expect("Invalid UTF-8");

        if !self.line.is_match(line) {
            return;
        }

        let highlighted = &mut out.bytes;
        let first = highlighted.len();
        let mut last_end = 0;

        for mat in self.line.find_iter(line) {
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

fn spinner() -> Result<ProgressBar> {
    let pb = ProgressBar::new_spinner();

    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(ProgressStyle::with_template("{msg} {spinner:.cyan.bold}")?.tick_chars("-\\|/"));

    Ok(pb)
}

fn main() -> Result<()> {
    let args = Args::parse();

    let databases = if args.all {
        Database::all()?
    } else {
        vec![Database::new(&args.database)?]
    };

    if args.create {
        for db in databases {
            db.create()?;
        }
    } else if args.update {
        for db in databases {
            db.update()?;
        }
    } else if args.info {
        Database::info()?;
    } else {
        let ignore_case = args.ignore_case;

        let patterns_re = args.pattern.iter().map(|p| Matcher::new(p, ignore_case));

        let bn_patterns_re = args
            .basename_pattern
            .unwrap_or_default()
            .iter()
            .map(|p| Matcher::basename(p, ignore_case))
            .collect::<Vec<_>>();

        let chained = &patterns_re.chain(bn_patterns_re).collect::<Vec<_>>();

        for db in databases {
            db.query(chained)?;
        }
    }

    Ok(())
}
