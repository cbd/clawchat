use rand::Rng;
use std::fs;
use std::io::Write;
use std::path::Path;

const KEY_LENGTH: usize = 32;

/// Generate a random API key as a hex string.
pub fn generate_key() -> String {
    let mut rng = rand::thread_rng();
    let bytes: Vec<u8> = (0..KEY_LENGTH).map(|_| rng.gen()).collect();
    hex::encode(bytes)
}

/// Encode bytes as hex string (simple implementation to avoid a dependency).
mod hex {
    pub fn encode(bytes: Vec<u8>) -> String {
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
}

/// Load or create the API key at the given path.
pub fn load_or_create_key(path: &Path) -> std::io::Result<String> {
    if path.exists() {
        harden_file_permissions(path)?;
        let key = fs::read_to_string(path)?.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    let key = generate_key();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_owner_only_atomic(path, &key)?;
    Ok(key)
}

/// Rotate the key: generate a new one and overwrite the file.
pub fn rotate_key(path: &Path) -> std::io::Result<String> {
    let key = generate_key();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_owner_only_atomic(path, &key)?;
    Ok(key)
}

fn write_owner_only_atomic(path: &Path, contents: &str) -> std::io::Result<()> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("auth.key");
    let temporary = path.with_file_name(format!(
        ".{file_name}.{}.{}.tmp",
        std::process::id(),
        &generate_key()[..8]
    ));
    let result = (|| {
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(contents.as_bytes())?;
        file.sync_all()?;
        harden_file_permissions(&temporary)?;
        fs::rename(&temporary, path)?;
        harden_file_permissions(path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

pub(crate) fn harden_file_permissions(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    #[cfg(unix)]
    fn key_create_rotate_and_existing_repair_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.key");
        let first = load_or_create_key(&path).unwrap();
        assert_eq!(mode(&path), 0o600);
        let second = rotate_key(&path).unwrap();
        assert_ne!(first, second);
        assert_eq!(mode(&path), 0o600);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(load_or_create_key(&path).unwrap(), second);
        assert_eq!(mode(&path), 0o600);
    }
}
