#![deny(clippy::all, clippy::pedantic, clippy::unwrap_used)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::{Path, PathBuf},
    process, str,
    sync::{Arc, Mutex, OnceLock},
    thread,
    time::{Duration, SystemTime},
};

use anyhow::{Context, Result};
use bstr::io::BufReadExt;
use clap::Parser;
use colored::Colorize;
use crossbeam_channel as channel;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use file_type_enum::FileType;
use fs_err as fs;
use fs4::fs_std::FileExt;
use home_dir::HomeDirExt;
use indicatif::{ProgressBar, ProgressStyle};
use lz4::EncoderBuilder;
use num_format::{Locale, ToFormattedString};
use regex::{Regex, RegexBuilder};
use serde::Deserialize;

static UPPER_RE: OnceLock<Regex> = OnceLock::new();

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
    pub files: usize,
}

impl Database {
    pub fn new(name: &str) -> Result<Self> {
        let config = Self::xdg_path(PathType::Config)?;
        let data = Self::xdg_path(PathType::Data)?;

        let config_path = config.join(name).join("config.toml");
        let database_path = data.join(name).join("db.lz4");

        let description = Self::config(name).unwrap_or_default().description;

        let lines = if database_path.exists() {
            let input_file = fs::File::open(&database_path)?;
            let decoder = lz4::Decoder::new(input_file)?;
            let reader = io::BufReader::new(decoder);

            reader.byte_lines().count()
        } else {
            0
        };

        Ok(Self {
            name: name.to_string(),
            description,
            root: config.join(name),
            config: config_path,
            ignores: config.join(name).join("ignores"),
            data: database_path,
            files: lines,
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
                    db.files.to_formatted_string(&Locale::en)
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

    fn is_apple_bundle(entry: &DirEntry) -> bool {
        if let Some(path_str) = entry.path().to_str() {
            return path_str.contains(".app/")
                || path_str.contains(".bundle/")
                || path_str.contains(".fcpbundle/")
                || path_str.contains(".framework/");
        }

        false
    }

    pub fn paths(&self) -> Result<Vec<PathBuf>> {
        let paths = Self::config(&self.name)?
            .paths
            .iter()
            .filter_map(|p| p.expand_home().ok())
            .collect::<Vec<PathBuf>>();
        Ok(paths)
    }

    pub fn query(&self, patterns_re: &[Regex]) -> Result<()> {
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

        let mut reader = io::BufReader::new(lz4::Decoder::new(fs::File::open(&self.data)?)?);
        let mut writer = io::BufWriter::new(io::stdout().lock());

        let mut matches: Vec<String> = vec![];

        reader.for_byte_line(|line_| {
            let line = str::from_utf8(line_).expect("Invalid UTF-8");

            for re in patterns_re {
                if !re.is_match(line) {
                    continue;
                }

                let mut highlighted = String::new();
                let mut last_end = 0;

                for mat in re.find_iter(line) {
                    highlighted.push_str(&line[last_end..mat.start()]);
                    highlighted.push_str("\x1b[31m");
                    highlighted.push_str(mat.as_str());
                    highlighted.push_str("\x1b[0m");
                    last_end = mat.end();
                }

                highlighted.push_str(&line[last_end..]);

                matches.push(highlighted);
            }

            Ok(true)
        })?;

        matches.sort();

        for m in matches {
            let _ = writer.write_all(m.as_bytes());
            let _ = writer.write_all(b"\n");
        }

        Ok(())
    }

    fn walker(&self) -> Result<ignore::WalkParallel> {
        let mut paths = self.paths()?.into_iter();

        let mut wd = ignore::WalkBuilder::new(paths.next().expect("No paths provided"));

        wd.hidden(false)
            .hidden(false)
            .follow_links(false)
            .git_global(true);

        for path in paths.filter(|path| path.exists()) {
            wd.add(path);
        }

        wd.add_ignore(&self.ignores);
        wd.threads(num_cpus::get());

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

        lock.lock_exclusive()?;

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
                        if FileType::symlink_read_at(&value).is_ok() {
                            writeln!(encoder, "{}", value.display())?;
                        }
                    }
                    WorkerResult::Error(err) => print_err!("{}", err.to_string()),
                }
            }
            let (_output, result) = encoder.finish();
            result
        });

        let cache_dir: Arc<Mutex<BTreeSet<PathBuf>>> = Arc::new(Mutex::new(BTreeSet::new()));

        self.walker()?.run(|| {
            let tx = tx.clone();
            let ig = cache_dir.clone();

            Box::new(move |e| {
                //: Result<ignore::DirEntry,ignore::Error>

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
                            return match tx.send(WorkerResult::Error(ignore::Error::WithPath {
                                path,
                                err: error,
                            })) {
                                Ok(()) => ignore::WalkState::Continue,
                                Err(_) => ignore::WalkState::Quit,
                            };
                        }
                    },
                    Err(err) => {
                        return match tx.send(WorkerResult::Error(err)) {
                            Ok(()) => ignore::WalkState::Continue,
                            Err(_) => ignore::WalkState::Quit,
                        };
                    }
                };

                if Self::is_apple_bundle(&entry) {
                    return ignore::WalkState::Continue;
                }

                if let Ok(mut ignored) = ig.lock() {
                    if cachedir::is_tagged(entry.path()).unwrap_or(false) {
                        ignored.insert(entry.path().to_owned());
                        return ignore::WalkState::Continue;
                    }

                    // Check if the parent directory is ignored.
                    if ignored
                        .iter()
                        .any(|ignored_path| entry.path().starts_with(ignored_path))
                    {
                        return ignore::WalkState::Continue;
                    }
                }

                match tx.send(WorkerResult::Entry(entry.path().to_owned())) {
                    Ok(()) => ignore::WalkState::Continue,
                    Err(_) => ignore::WalkState::Quit,
                }
            })
        });

        drop(tx);

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

fn build_regex(pattern: &str, ignore_case: bool) -> Regex {
    let re = UPPER_RE.get_or_init(|| Regex::new(r"[[:upper:]]").expect("Unable to compile regex"));

    match RegexBuilder::new(pattern)
        .case_insensitive(ignore_case || !re.is_match(pattern))
        .build()
    {
        Ok(re) => re,
        Err(e) => panic!("invalid regex: {e}"),
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

        let patterns_re = args.pattern.iter().map(|p| build_regex(p, ignore_case));

        let bn_patterns_re = args
            .basename_pattern
            .unwrap_or_default()
            .iter()
            .map(|p| build_regex(&format!("/[^/]*{p}[^/]*$"), ignore_case))
            .collect::<Vec<_>>();

        let chained = &patterns_re.chain(bn_patterns_re).collect::<Vec<_>>();

        for db in databases {
            db.query(chained)?;
        }
    }

    Ok(())
}
