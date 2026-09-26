use std::{env, fs, path::PathBuf};

use anyhow::{bail, Context, Result};
use turso_mysql::MySqlDatabaseCatalog;

fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let root = PathBuf::from(args.next().context("expected a disposable data root")?);
    let name = args.next().context("expected a database name")?;
    if args.next().is_some() {
        bail!("expected exactly a data root and database name");
    }
    let name = name.to_str().context("database name must be valid UTF-8")?;
    if !root.is_dir() || fs::read_dir(&root)?.next().is_some() {
        bail!("disposable data root must be an existing empty directory");
    }
    let catalog = MySqlDatabaseCatalog::open(&root)?;
    let created = catalog.create(name)?;
    if created != name {
        bail!("database name was canonicalized unexpectedly");
    }
    println!("created disposable MySQL database {created}");
    Ok(())
}
