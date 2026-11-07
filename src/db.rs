use std::{io::Write, path::PathBuf, process};

use anyhow::{Context, Result};
use colored::Colorize;
use figment::{
    Figment,
    providers::{Format, Toml},
};
use fs_err as fs;
use home_dir::HomeDirExt;
use num_format::{Locale, ToFormattedString};
use serde::Deserialize;

use crate::frame::{self, Consume};

const CHUNK_SIZE: usize = 1 << 21;

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
        Ok(self
            .scan(|| LineCount(0))?
            .iter()
            .map(|count| count.0)
            .sum())
    }

    /// Decompresses and verifies the database, feeding its lines to consumers made by
    /// `new_consumer`, and returns them.
    pub fn scan<C: Consume>(&self, new_consumer: impl Fn() -> C + Sync) -> Result<Vec<C>> {
        let file = fs::File::open(&self.data)?;
        Ok(frame::decode(file, CHUNK_SIZE, &new_consumer)?)
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
}

struct LineCount(usize);

impl Consume for LineCount {
    fn consume(&mut self, lines: &[u8]) {
        self.0 += memchr::memchr_iter(b'\n', lines).count()
            + usize::from(lines.last().is_some_and(|&b| b != b'\n'));
    }
}
