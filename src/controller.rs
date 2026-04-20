use std::{
    error::Error,
    fs::{self, File, OpenOptions, remove_file},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use safe_path::scoped_join;
use thiserror::Error;

use crate::lock::LockFile;

#[derive(Error, Debug)]
pub enum RaptorBoostError {
    #[error("path {0} is not clean")]
    PathSanitization(String),
    #[error("couldn't lock")]
    LockFailure,
    #[error("transfer already complete")]
    TransferAlreadyComplete,
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error("error renaming file: `{0}`")]
    RenameError(String),
    #[error("other error: `{0}`")]
    OtherError(String),
}

#[derive(Error, Debug)]
#[error("{0}")]
pub struct RaptorBoostControllerError(String);

pub struct RaptorBoostController {
    partial_dir: PathBuf,
    complete_dir: PathBuf,
    transfers_dir: PathBuf,
    lock_dir: PathBuf,
}

pub enum CheckFileResult {
    FileComplete,
    FilePartialOffset(u64),
}

pub struct RaptorBoostTransfer {
    sha256sum: String,
    complete_path: PathBuf,
    partial_path: PathBuf,
    f: File,
    _l: LockFile,
    hasher: ring::digest::Context,
}

impl RaptorBoostTransfer {
    pub fn write_all(&mut self, d: &[u8]) -> io::Result<()> {
        self.f.write_all(d)?;
        self.hasher.update(d);
        Ok(())
    }

    pub fn complete(self) -> Result<(), RaptorBoostError> {
        let calc_sha256sum = hex::encode(self.hasher.finish());

        if self.sha256sum != calc_sha256sum {
            let _ = remove_file(&self.partial_path);
            return Err(RaptorBoostError::ChecksumMismatch);
        }

        fs::rename(&self.partial_path, &self.complete_path).map_err(|e| {
            let _ = remove_file(&self.partial_path);
            RaptorBoostError::RenameError(e.to_string())
        })
    }
}

impl RaptorBoostController {
    pub fn new(output_dir: &Path) -> Result<RaptorBoostController, Box<dyn Error>> {
        if !output_dir.try_exists()? {
            return Err(Box::new(RaptorBoostControllerError(
                "output directory doesn't exist".to_string(),
            )));
        }

        let partial_dir = output_dir.join("partial");
        if !partial_dir.exists() {
            fs::create_dir(&partial_dir)?;
        }

        let complete_dir = output_dir.join("complete");
        if !complete_dir.exists() {
            fs::create_dir(&complete_dir)?;
        }

        let transfers_dir = output_dir.join("transfers");
        if !transfers_dir.exists() {
            fs::create_dir(&transfers_dir)?;
        }

        let lock_dir = output_dir.join("lock");
        if lock_dir.exists() {
            fs::remove_dir_all(&lock_dir)?;
        }
        fs::create_dir(&lock_dir)?;

        Ok(RaptorBoostController {
            partial_dir,
            complete_dir,
            transfers_dir,
            lock_dir,
        })
    }

    pub fn start_transfer(
        &self,
        sha256sum: &str,
        force: bool,
    ) -> Result<RaptorBoostTransfer, RaptorBoostError> {
        let partial_lock_path = scoped_join(self.get_lock_dir(), sha256sum)
            .map_err(|_| RaptorBoostError::PathSanitization(sha256sum.to_string()))?;

        if force {
            let _ = remove_file(&partial_lock_path);
        }

        let partial_lock =
            LockFile::open(partial_lock_path).map_err(|_| RaptorBoostError::LockFailure)?;

        if let CheckFileResult::FileComplete = self.check_file(sha256sum)? {
            return Err(RaptorBoostError::TransferAlreadyComplete);
        }

        let partial_path = self.partial_dir.join(sha256sum);
        let mut f = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&partial_path)
            .map_err(|e| RaptorBoostError::OtherError(e.to_string()))?;

        f.seek(SeekFrom::Start(0))
            .map_err(|e| RaptorBoostError::OtherError(e.to_string()))?;

        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 8192];
        loop {
            match f.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buffer[..n]),
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            }
        }

        f.seek(SeekFrom::End(0))
            .map_err(|e| RaptorBoostError::OtherError(e.to_string()))?;

        Ok(RaptorBoostTransfer {
            f,
            _l: partial_lock,
            hasher,
            sha256sum: sha256sum.to_owned(),
            complete_path: self.complete_dir.join(sha256sum),
            partial_path,
        })
    }

    pub fn get_partial_dir(&self) -> &Path {
        &self.partial_dir
    }

    pub fn get_complete_dir(&self) -> &Path {
        &self.complete_dir
    }

    pub fn get_lock_dir(&self) -> &Path {
        &self.lock_dir
    }

    pub fn get_transfers_dir(&self) -> &Path {
        &self.transfers_dir
    }

    pub fn get_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    pub fn check_file(&self, sha256sum: &str) -> Result<CheckFileResult, RaptorBoostError> {
        let full_complete_file = scoped_join(self.get_complete_dir(), sha256sum)
            .map_err(|_| RaptorBoostError::PathSanitization(sha256sum.to_string()))?;

        if full_complete_file.exists() {
            return Ok(CheckFileResult::FileComplete);
        }

        let full_partial_file = scoped_join(self.get_partial_dir(), sha256sum)
            .map_err(|_| RaptorBoostError::PathSanitization(sha256sum.to_string()))?;

        if full_partial_file.exists() {
            let offset = fs::metadata(&full_partial_file)
                .map_err(|e| RaptorBoostError::OtherError(e.to_string()))?
                .len();
            return Ok(CheckFileResult::FilePartialOffset(offset));
        }

        Ok(CheckFileResult::FilePartialOffset(0))
    }
}
