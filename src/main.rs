#![deny(clippy::all, clippy::pedantic, clippy::unwrap_used)]
#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]

use std::process;

use anyhow::Result;
use clap::Parser;
use colored::Colorize;

macro_rules! print_err {
    ($($err:tt)*) => ({
        eprintln!("{}: {}", "Error".red().bold(), format!($($err)*));
    })
}

mod db;
mod frame;
mod index;
mod query;

use db::Database;
use query::{Matcher, Patterns};

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

fn main() {
    if let Err(e) = run(Args::parse()) {
        print_err!("{e:#}");
        process::exit(1);
    }
}

fn run(args: Args) -> Result<()> {
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

        let chained = &Patterns::new(patterns_re.chain(bn_patterns_re).collect());

        for db in databases {
            db.query(chained)?;
        }
    }

    Ok(())
}
