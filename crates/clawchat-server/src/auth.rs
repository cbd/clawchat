use rand::Rng;
use std::fs;
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
        let key = fs::read_to_string(path)?.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    let key = generate_key();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &key)?;
    Ok(key)
}

/// Rotate the key: generate a new one and overwrite the file.
pub fn rotate_key(path: &Path) -> std::io::Result<String> {
    let key = generate_key();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &key)?;
    Ok(key)
}
