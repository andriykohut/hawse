use std::path::PathBuf;

use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::config_file;

pub fn run(config: Option<PathBuf>, out: Option<PathBuf>) -> miette::Result<()> {
    let path = match out {
        Some(path) => path,
        None => config_file::client_key_path(config)?,
    };
    let (identity, created) = Identity::load_or_create(&path)
        .into_diagnostic()
        .wrap_err_with(|| format!("cannot create a key at {}", path.display()))?;
    if created {
        tracing::info!(path = %path.display(), "created key");
    } else {
        tracing::info!(path = %path.display(), "key already exists");
    }
    println!("{}", identity.public_key());
    Ok(())
}
