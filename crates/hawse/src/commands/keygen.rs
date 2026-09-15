use std::path::PathBuf;

use hawse_core::identity::Identity;
use miette::{IntoDiagnostic as _, WrapErr as _};

use crate::paths::{self, Role};

pub fn run(out: Option<PathBuf>) -> miette::Result<()> {
    let path = out.unwrap_or_else(|| {
        paths::locate(Role::Client, None)
            .dir
            .join(Role::Client.key_name())
    });
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
