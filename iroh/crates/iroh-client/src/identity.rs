//! Persistent Iroh endpoint identity. The endpoint id that clients pair with is derived from it.

use std::fs;
use std::io;
use std::path::Path;

use iroh::SecretKey;

use crate::fsutil::{restrict_permissions, write_private_atomic};

pub const IDENTITY_FILE: &str = "identity.key";
const KEY_BYTES: usize = 32;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error(
        "{path} is not a valid identity key ({len} bytes, expected {expected}); refusing to replace it \
         because a new identity would invalidate every paired device",
        expected = KEY_BYTES
    )]
    Corrupt { path: String, len: usize },
    #[error("could not access identity key {path}: {source}")]
    Io { path: String, source: io::Error },
}

/// Loads the identity at `path`, creating it on first run. Existing keys are never regenerated.
pub fn load_or_create(path: &Path) -> Result<SecretKey, IdentityError> {
    let io_error = |source: io::Error| IdentityError::Io {
        path: path.display().to_string(),
        source,
    };
    match fs::read(path) {
        Ok(bytes) => {
            let key: [u8; KEY_BYTES] =
                bytes
                    .as_slice()
                    .try_into()
                    .map_err(|_| IdentityError::Corrupt {
                        path: path.display().to_string(),
                        len: bytes.len(),
                    })?;
            restrict_permissions(path).map_err(io_error)?;
            Ok(SecretKey::from_bytes(&key))
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let secret = SecretKey::generate();
            write_private_atomic(path, &secret.to_bytes()).map_err(io_error)?;
            Ok(secret)
        }
        Err(error) => Err(io_error(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fsutil::test_support::TempDir;

    #[test]
    fn identity_is_stable_across_loads() {
        let dir = TempDir::create();
        let path = dir.0.join(IDENTITY_FILE);
        let first = load_or_create(&path).unwrap();
        let second = load_or_create(&path).unwrap();
        assert_eq!(first.public(), second.public());
    }

    #[test]
    fn corrupt_identity_is_not_overwritten() {
        let dir = TempDir::create();
        let path = dir.0.join(IDENTITY_FILE);
        fs::write(&path, b"short").unwrap();
        assert!(matches!(
            load_or_create(&path),
            Err(IdentityError::Corrupt { len: 5, .. })
        ));
        assert_eq!(fs::read(&path).unwrap(), b"short");
    }
}
