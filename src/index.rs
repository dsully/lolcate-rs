use std::{
    collections::HashSet,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, SystemTime},
};

use anyhow::Result;
use colored::Colorize;
use crossbeam_channel as channel;
use file_locking::FileExt as _;
use fs_err as fs;
use fs4::FileExt;
use indicatif::{ProgressBar, ProgressStyle};
use lz4::EncoderBuilder;

use crate::db::Database;

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

impl Database {
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

        // Queries keep reading the previous database until the new one is renamed into place.
        let partial = self.data.with_extension("lz4.partial");
        let database = fs::File::create(&partial)?;

        let (tx, rx) = channel::bounded::<WorkerResult>(8 * 1024);

        let s = spinner()?;
        let start_time = SystemTime::now();

        s.set_message(format!("{} {}...", "Updating".green().bold(), self.name));

        let stdout_thread = thread::spawn(move || {
            let mut encoder = EncoderBuilder::new()
                .level(3)
                // Independent, checksummed blocks are decoded and verified in parallel by queries.
                .block_mode(lz4::BlockMode::Independent)
                .block_size(lz4::BlockSize::Max1MB)
                .block_checksum(lz4::liblz4::BlockChecksum::BlockChecksumEnabled)
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

        // Bundle directories are decided here, so the walker never reads them. Other directories are
        // decided when visited, right after the walker has read them: probing a directory whose
        // metadata was just read is far cheaper than probing it from its parent.
        let filter = {
            let indexer = indexer.clone();
            move |e: &ignore::DirEntry| {
                if !(e.file_type().is_some_and(|t| t.is_dir()) && is_bundle_dir(e.path())) {
                    return true;
                }
                let _ = indexer.visit(e.path(), e.file_type(), false);
                false
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
                indexer.visit(entry.path(), entry.file_type(), root)
            })
        });

        // The writer finishes once the last sender, held by the indexer, is gone.
        drop(indexer);

        stdout_thread.join().expect("Unable to join threads")?;
        fs::rename(&partial, &self.data)?;

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

/// One `lstat` both checks `dir` for a `CACHEDIR.TAG` and, since that needs search permission on
/// `dir`, whether its entries can be stat'ed. The tag is opened only if it exists: `open` costs far
/// more than `lstat` here, even for a missing file.
fn probe_dir(dir: &Path) -> DirProbe {
    let tag = dir.join("CACHEDIR.TAG");
    match tag.symlink_metadata() {
        Ok(_) => {
            let mut header = [0u8; cachedir::HEADER.len()];
            if std::fs::File::open(&tag).is_ok_and(|mut tag| {
                tag.read(&mut header)
                    .is_ok_and(|n| n == header.len() && header == *cachedir::HEADER)
            }) {
                DirProbe::Tagged
            } else {
                DirProbe::Untagged
            }
        }
        Err(e) if e.kind() == io::ErrorKind::PermissionDenied => DirProbe::Unsearchable,
        Err(_) => DirProbe::Untagged,
    }
}

fn spinner() -> Result<ProgressBar> {
    let pb = ProgressBar::new_spinner();

    pb.enable_steady_tick(Duration::from_millis(100));

    pb.set_style(ProgressStyle::with_template("{msg} {spinner:.cyan.bold}")?.tick_chars("-\\|/"));

    Ok(pb)
}
