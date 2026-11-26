//! Complete stored-byte evidence from a single opened regular file (ADR-031).
use crate::tool_budget::{BudgetHit, ToolBudget};
use sha2::{Digest, Sha256};
use std::fs::{File, Metadata};
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;

#[derive(Debug)]
pub enum ReadError {
    Io(io::Error),
    Limit(BudgetHit),
    Changed,
}

impl From<io::Error> for ReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<BudgetHit> for ReadError {
    fn from(hit: BudgetHit) -> Self {
        Self::Limit(hit)
    }
}

fn stamp(meta: &Metadata) -> (u64, i64, i64, i64, i64) {
    (
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}

/// Never reopen a path to obtain the digest. Charge bytes as they are read,
/// not a second time for hashing or for the length of the returned page.
pub fn read(mut file: File, budget: Option<&ToolBudget>) -> Result<(Vec<u8>, String), ReadError> {
    let before = file.metadata()?;
    if !before.is_file() {
        return Err(io::Error::other("read digest requires a regular file").into());
    }
    if let Some(b) = budget {
        b.check_deadline()?;
        b.precheck_input(before.len())?;
    }
    let mut bytes = Vec::new();
    let mut hash = Sha256::new();
    let mut chunk = [0u8; 32 * 1024];
    loop {
        if let Some(b) = budget {
            b.check_deadline()?;
        }
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        if let Some(b) = budget {
            b.charge_input(n as u64)?;
        }
        if (bytes.len() as u64).saturating_add(n as u64) > before.len() {
            return Err(ReadError::Changed);
        }
        hash.update(&chunk[..n]);
        bytes.extend_from_slice(&chunk[..n]);
    }
    verify_read(&before, &file.metadata()?, bytes.len() as u64)?;
    if let Some(b) = budget {
        b.check_deadline()?;
    }
    Ok((bytes, format!("{:x}", hash.finalize())))
}

fn verify_read(before: &Metadata, after: &Metadata, bytes: u64) -> Result<(), ReadError> {
    if stamp(before) == stamp(after) && before.len() == bytes {
        Ok(())
    } else {
        Err(ReadError::Changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_uses_the_open_file_even_after_path_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("object");
        std::fs::write(&path, b"abc").unwrap();
        let opened = File::open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("old")).unwrap();
        std::fs::write(path, b"replacement").unwrap();
        let (bytes, digest) = read(opened, None).unwrap();
        assert_eq!(bytes, b"abc");
        assert_eq!(
            digest,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn refuse_changed_evidence_before_publishing_a_digest() {
        let mut file = tempfile::tempfile().unwrap();
        use std::io::Write;
        file.write_all(b"before").unwrap();
        let before = file.metadata().unwrap();
        assert!(matches!(
            verify_read(&before, &before, 3),
            Err(ReadError::Changed)
        ));
        file.write_all(b"after").unwrap();
        assert!(matches!(
            verify_read(&before, &file.metadata().unwrap(), 6),
            Err(ReadError::Changed)
        ));
    }
}
