use anyhow::{Context as _, Result};
use std::collections::HashMap;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};

pub fn zsh_integration_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("shell_integration").join("zsh")
}

/// Writes the bundled zsh files to [`zsh_integration_dir`] if missing or changed.
pub fn install_zsh_integration(data_dir: &Path, files: &[(&str, &[u8])]) -> Result<PathBuf> {
    let directory = zsh_integration_dir(data_dir);
    std::fs::create_dir_all(&directory)
        .with_context(|| format!("creating {}", directory.display()))?;
    for (name, contents) in files {
        let path = directory.join(name);
        if std::fs::read(&path).ok().as_deref() == Some(*contents) {
            continue;
        }
        std::fs::write(&path, contents).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(directory)
}

/// If `shell_program` is zsh: sets ZDOTDIR to the installed dir, PHOTON_USER_ZDOTDIR to the user's
/// previous ZDOTDIR (if any) and PHOTON_BLOCKS=1, and returns true. Otherwise leaves env alone and returns false.
pub fn prepare_zsh_env<S: BuildHasher>(
    shell_program: &str,
    integration_dir: &Path,
    env: &mut HashMap<String, String, S>,
) -> bool {
    if Path::new(shell_program)
        .file_name()
        .and_then(|name| name.to_str())
        != Some("zsh")
    {
        return false;
    }
    let integration_dir = integration_dir.to_string_lossy().into_owned();
    let previous = env
        .get("ZDOTDIR")
        .cloned()
        .or_else(|| std::env::var("ZDOTDIR").ok())
        .filter(|zdotdir| !zdotdir.is_empty());
    match previous {
        // A zsh started from inside a Photon terminal already points at our dir;
        // keep the user's directory it recorded.
        Some(zdotdir) if zdotdir == integration_dir => {
            if let Ok(user) = std::env::var("PHOTON_USER_ZDOTDIR") {
                env.entry("PHOTON_USER_ZDOTDIR".into()).or_insert(user);
            }
        }
        // Empty (not absent) so a value inherited from the process environment cannot leak in.
        other => {
            env.insert("PHOTON_USER_ZDOTDIR".into(), other.unwrap_or_default());
        }
    }
    env.insert("ZDOTDIR".into(), integration_dir);
    env.insert("PHOTON_BLOCKS".into(), "1".into());
    // Empty so the shell records its own terminal, not one inherited from a Photon launched inside Photon.
    env.insert("PHOTON_TTY".into(), String::new());
    true
}
