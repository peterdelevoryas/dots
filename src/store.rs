//! Bundles on disk. Each push is a gzipped tar kept as
//! `bundles/<version>.tar.gz` with its SHA-256 beside it in
//! `bundles/<version>.sha256`; the file `current` names the version that
//! `/install` hands out. Versions sort by time, so the newest name is the
//! newest push.

use std::{
    fs,
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};

/// The largest bundle a push may upload.
pub const MAX_BUNDLE: usize = 64 * 1024 * 1024;
/// How many versions to keep. The current one is never pruned.
const KEEP: usize = 30;

#[derive(Debug, Clone, Serialize)]
pub struct Bundle {
    pub version: String,
    pub sha256: String,
    pub size: u64,
    pub current: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum PutError {
    #[error("invalid bundle: {0}")]
    Invalid(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Clone)]
pub struct Store {
    dir: PathBuf,
    /// Serializes pushes and `current` changes.
    lock: Arc<Mutex<()>>,
}

impl Store {
    pub fn open(dir: &Path) -> Result<Self> {
        fs::create_dir_all(dir.join("bundles"))
            .with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lock: Arc::new(Mutex::new(())),
        })
    }

    /// Stores a new bundle and makes it current.
    pub fn put(&self, bytes: &[u8]) -> Result<Bundle, PutError> {
        if bytes.len() > MAX_BUNDLE {
            return Err(PutError::Invalid(format!(
                "{} bytes is over the {MAX_BUNDLE}-byte limit",
                bytes.len()
            )));
        }
        validate(bytes)?;

        let sha256 = hex::encode(Sha256::digest(bytes));
        let now = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
        let version = format!("{now}-{}", &sha256[..8]);

        let _guard = self.lock.lock().unwrap();
        write_atomic(&self.bundle_path(&version), bytes)?;
        write_atomic(&self.sha_path(&version), sha256.as_bytes())?;
        self.write_current(&version)?;
        self.prune(&version)?;
        tracing::info!(version, size = bytes.len(), "stored bundle");
        Ok(Bundle {
            version,
            sha256,
            size: bytes.len() as u64,
            current: true,
        })
    }

    /// Reads a bundle: the named version, or the current one if `None`.
    /// Returns `None` if there is no such version (or nothing pushed yet).
    pub fn get(&self, version: Option<&str>) -> Result<Option<(Bundle, Vec<u8>)>> {
        let current = self.current()?;
        let version = match version {
            Some(v) => v.to_string(),
            None => match &current {
                Some(v) => v.clone(),
                None => return Ok(None),
            },
        };
        if !valid_version(&version) {
            return Ok(None);
        }
        let bytes = match fs::read(self.bundle_path(&version)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        let sha256 = fs::read_to_string(self.sha_path(&version))?;
        let bundle = Bundle {
            current: current.as_deref() == Some(version.as_str()),
            size: bytes.len() as u64,
            sha256: sha256.trim().to_string(),
            version,
        };
        Ok(Some((bundle, bytes)))
    }

    /// Every stored version, newest first.
    pub fn list(&self) -> Result<Vec<Bundle>> {
        let current = self.current()?;
        let mut bundles = Vec::new();
        for version in self.versions()? {
            let size = fs::metadata(self.bundle_path(&version))?.len();
            let sha256 = fs::read_to_string(self.sha_path(&version))?;
            bundles.push(Bundle {
                current: current.as_deref() == Some(version.as_str()),
                sha256: sha256.trim().to_string(),
                size,
                version,
            });
        }
        bundles.reverse();
        Ok(bundles)
    }

    /// Points `current` at an existing version. Returns false if there is no
    /// such version.
    pub fn set_current(&self, version: &str) -> Result<bool> {
        if !valid_version(version) {
            return Ok(false);
        }
        let _guard = self.lock.lock().unwrap();
        if !self.bundle_path(version).exists() {
            return Ok(false);
        }
        self.write_current(version)?;
        tracing::info!(version, "set current bundle");
        Ok(true)
    }

    fn current(&self) -> Result<Option<String>> {
        match fs::read_to_string(self.dir.join("current")) {
            Ok(v) => Ok(Some(v.trim().to_string())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn write_current(&self, version: &str) -> Result<()> {
        write_atomic(&self.dir.join("current"), format!("{version}\n").as_bytes())
    }

    /// Stored versions, oldest first.
    fn versions(&self) -> Result<Vec<String>> {
        let mut versions = Vec::new();
        for entry in fs::read_dir(self.dir.join("bundles"))? {
            let name = entry?.file_name();
            let Some(name) = name.to_str() else { continue };
            if let Some(version) = name.strip_suffix(".tar.gz") {
                versions.push(version.to_string());
            }
        }
        versions.sort();
        Ok(versions)
    }

    fn prune(&self, current: &str) -> Result<()> {
        let versions = self.versions()?;
        if versions.len() <= KEEP {
            return Ok(());
        }
        let excess = versions.len() - KEEP;
        for version in &versions[..excess] {
            if version == current {
                continue;
            }
            fs::remove_file(self.bundle_path(version))?;
            let _ = fs::remove_file(self.sha_path(version));
            tracing::info!(version, "pruned bundle");
        }
        Ok(())
    }

    fn bundle_path(&self, version: &str) -> PathBuf {
        self.dir.join("bundles").join(format!("{version}.tar.gz"))
    }

    fn sha_path(&self, version: &str) -> PathBuf {
        self.dir.join("bundles").join(format!("{version}.sha256"))
    }
}

/// Versions come from URLs, so only allow the characters we generate.
fn valid_version(v: &str) -> bool {
    !v.is_empty() && v.len() <= 64 && v.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// Checks that `bytes` is a gzipped tar with `setup.sh` at its root, and that
/// every entry is a plain file, directory, or symlink that stays inside the
/// archive.
fn validate(bytes: &[u8]) -> Result<(), PutError> {
    let invalid = |msg: String| PutError::Invalid(msg);
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    let entries = archive
        .entries()
        .map_err(|e| invalid(format!("not a gzipped tar: {e}")))?;
    let mut has_setup = false;
    for entry in entries {
        let mut entry = entry.map_err(|e| invalid(format!("reading archive: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| invalid(format!("bad entry path: {e}")))?
            .into_owned();
        if !contained(&path) {
            return Err(invalid(format!("{} escapes the archive", path.display())));
        }
        let kind = entry.header().entry_type();
        if kind.is_symlink() {
            let target = entry
                .link_name()
                .map_err(|e| invalid(format!("bad link in {}: {e}", path.display())))?;
            if let Some(target) = target
                && target.is_absolute()
            {
                return Err(invalid(format!(
                    "{} is a symlink to an absolute path",
                    path.display()
                )));
            }
        } else if !kind.is_file() && !kind.is_dir() {
            return Err(invalid(format!(
                "{} is not a file, directory, or symlink",
                path.display()
            )));
        }
        if kind.is_file() && normalize(&path) == Path::new("setup.sh") {
            has_setup = true;
        }
        // Read the whole entry so a truncated archive fails here.
        std::io::copy(&mut entry, &mut std::io::sink())
            .map_err(|e| invalid(format!("reading {}: {e}", path.display())))?;
    }
    if !has_setup {
        return Err(invalid("no setup.sh at the archive root".to_string()));
    }
    Ok(())
}

fn contained(path: &Path) -> bool {
    for component in path.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    true
}

/// Drops `.` components, so `./setup.sh` and `setup.sh` compare equal.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        if let Component::Normal(part) = component {
            out.push(part);
        }
    }
    out
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = fs::File::create(&tmp).with_context(|| format!("creating {}", tmp.display()))?;
    file.write_all(bytes)?;
    file.sync_all()?;
    fs::rename(&tmp, path).with_context(|| format!("renaming to {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
pub mod tests {
    use super::*;

    pub fn temp_dir(name: &str) -> PathBuf {
        let suffix: [u8; 8] = rand::random();
        let dir = std::env::temp_dir().join(format!("dots-{name}-{}", hex::encode(suffix)));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Builds a gzipped tar from (path, contents) pairs.
    pub fn tarball(files: &[(&str, &str)]) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        for (path, contents) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, path, contents.as_bytes())
                .unwrap();
        }
        builder.into_inner().unwrap().finish().unwrap()
    }

    /// A valid bundle plus one entry with a raw path, bypassing the builder's
    /// own path checks.
    fn tarball_with_raw_path(path: &str) -> Vec<u8> {
        let gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, "setup.sh", std::io::empty())
            .unwrap();

        let mut header = tar::Header::new_gnu();
        header.as_gnu_mut().unwrap().name[..path.len()].copy_from_slice(path.as_bytes());
        header.set_size(0);
        header.set_mode(0o644);
        header.set_cksum();
        builder.append(&header, std::io::empty()).unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    #[test]
    fn put_get_list_and_rollback() -> Result<()> {
        let store = Store::open(&temp_dir("store"))?;
        assert!(store.get(None)?.is_none());

        let first = store.put(&tarball(&[("setup.sh", "echo 1"), ("home/.zshrc", "a")]))?;
        std::thread::sleep(std::time::Duration::from_millis(1100));
        let second = store.put(&tarball(&[("./setup.sh", "echo 2")]))?;
        assert_ne!(first.version, second.version);

        let (bundle, bytes) = store.get(None)?.unwrap();
        assert_eq!(bundle.version, second.version);
        assert_eq!(hex::encode(Sha256::digest(&bytes)), second.sha256);

        let list = store.list()?;
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].version, second.version);
        assert!(list[0].current && !list[1].current);

        assert!(store.set_current(&first.version)?);
        assert_eq!(store.get(None)?.unwrap().0.version, first.version);
        assert!(!store.set_current("nope")?);
        assert!(!store.set_current("../../etc/passwd")?);
        assert!(store.get(Some("../current"))?.is_none());
        Ok(())
    }

    #[test]
    fn rejects_bad_bundles() {
        let store = Store::open(&temp_dir("reject")).unwrap();
        let cases = [
            ("not gzip", b"hello".to_vec()),
            ("no setup", tarball(&[("home/.zshrc", "a")])),
            ("setup nested", tarball(&[("home/setup.sh", "a")])),
            ("parent dir", tarball_with_raw_path("../evil")),
            ("absolute", tarball_with_raw_path("/etc/evil")),
        ];
        for (name, bytes) in cases {
            match store.put(&bytes) {
                Err(PutError::Invalid(_)) => {}
                other => panic!("{name}: expected Invalid, got {other:?}"),
            }
        }
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn prune_keeps_current() -> Result<()> {
        let dir = temp_dir("prune");
        let store = Store::open(&dir)?;
        // Fake old versions directly on disk rather than waiting a second per push.
        for i in 0..KEEP + 5 {
            let version = format!("20200101T0000{i:02}Z-00000000");
            write_atomic(&store.bundle_path(&version), b"x")?;
            write_atomic(&store.sha_path(&version), b"x")?;
        }
        let oldest = "20200101T000000Z-00000000";
        store.write_current(oldest)?;
        store.prune(oldest)?;
        let versions = store.versions()?;
        assert!(versions.contains(&oldest.to_string()));
        assert_eq!(versions.len(), KEEP + 1);
        Ok(())
    }
}
