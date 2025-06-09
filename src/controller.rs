use std::{
    error::Error,
    fs::{self, File, OpenOptions, remove_file},
    io::{self, ErrorKind, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

use safe_path::scoped_join;
use thiserror::Error;

use crate::lock::LockFile;

// TODO: figure out these errors. they don't work well when used with both check_file and start_transfer
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
    pub fn write(&mut self, d: &[u8]) -> Result<usize, std::io::Error> {
        let res = self.f.write(d);

        if res.is_ok() {
            self.hasher.update(&d)
        }

        return res;
    }

    pub fn complete(self) -> Result<(), RaptorBoostError> {
        let calc_sha256sum: String = hex::encode(self.hasher.finish());

        if self.sha256sum != calc_sha256sum {
            let _ = remove_file(&self.partial_path).is_err();
            return Err(RaptorBoostError::ChecksumMismatch);
        }

        match std::fs::rename(&self.partial_path, &self.complete_path) {
            Ok(_) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&self.partial_path); // nothing we can do if this fails
                Err(RaptorBoostError::RenameError(e.to_string()))
            }
        }
    }
}

impl RaptorBoostController {
    pub fn new(output_dir: &PathBuf) -> Result<RaptorBoostController, Box<dyn Error>> {
        // base dir must exist
        if !output_dir.try_exists()? {
            return Err(Box::new(RaptorBoostControllerError(
                "output directory doesn't exist".to_string(),
            )));
        }

        // ensure directories exist
        let partial_dir = output_dir.as_path().join("partial");

        if !partial_dir.exists() {
            fs::create_dir(&partial_dir)?;
        }

        let complete_dir = output_dir.as_path().join("complete");

        if !complete_dir.exists() {
            fs::create_dir(&complete_dir)?;
        }

        let transfers_dir = output_dir.as_path().join("transfers");

        if !transfers_dir.exists() {
            fs::create_dir(&transfers_dir)?;
        }

        let lock_dir = output_dir.as_path().join("lock");

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
        // lock partial
        let partial_lock_path = match scoped_join(self.get_lock_dir(), &sha256sum) {
            Ok(p) => p,
            Err(_) => return Err(RaptorBoostError::PathSanitization(sha256sum.to_string())),
        };

        if force {
            let _ = remove_file(&partial_lock_path); // ignoring this because if it fails, the lock below will fail
        }

        let partial_lock = match LockFile::open(partial_lock_path.to_owned()) {
            Ok(l) => l,
            Err(e) => {
                println!("error locking {}: {}", partial_lock_path.display(), e);
                return Err(RaptorBoostError::LockFailure);
            }
        };

        // check this file's state
        let file_state = match self.check_file(&sha256sum) {
            Ok(s) => s,
            Err(e) => return Err(e),
        };

        match file_state {
            CheckFileResult::FileComplete => return Err(RaptorBoostError::TransferAlreadyComplete),
            _ => (),
        }

        // start writing partial file
        let partial_path = self.partial_dir.join(&sha256sum);
        let mut f = match OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&partial_path)
        {
            Ok(f) => f,
            Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
        };

        // calculate initial checksum
        match f.seek(io::SeekFrom::Start(0)) {
            Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            _ => (),
        }

        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0; 8192];
        loop {
            match f.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            }
        }

        // jump to end
        match f.seek(io::SeekFrom::End(0)) {
            Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            _ => (),
        }

        Ok(RaptorBoostTransfer {
            f,
            _l: partial_lock,
            hasher,
            sha256sum: sha256sum.to_owned(),
            complete_path: self.complete_dir.join(&sha256sum),
            partial_path,
        })
    }

    pub fn get_partial_dir(&self) -> &Path {
        self.partial_dir.as_path()
    }

    pub fn get_complete_dir(&self) -> &Path {
        return self.complete_dir.as_path();
    }

    pub fn get_lock_dir(&self) -> &Path {
        return self.lock_dir.as_path();
    }

    pub fn get_transfers_dir(&self) -> &Path {
        return self.transfers_dir.as_path();
    }

    pub fn get_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    pub fn check_file(&self, sha256sum: &str) -> Result<CheckFileResult, RaptorBoostError> {
        // first look for file in complete
        let full_complete_file = match scoped_join(self.get_complete_dir(), &sha256sum) {
            Ok(f) => f,
            Err(_) => return Err(RaptorBoostError::PathSanitization(sha256sum.to_string())),
        };

        if full_complete_file.exists() {
            return Ok(CheckFileResult::FileComplete);
        }

        // what about partial?
        let full_partial_file = match scoped_join(self.get_partial_dir(), &sha256sum) {
            Ok(f) => f,
            Err(_) => return Err(RaptorBoostError::PathSanitization(sha256sum.to_string())),
        };

        if full_partial_file.exists() {
            let mut f = match File::open(full_partial_file) {
                Ok(f) => f,
                Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            };

            let offset = match f.seek(SeekFrom::End(0)) {
                Ok(o) => o,
                Err(e) => return Err(RaptorBoostError::OtherError(e.to_string())),
            };

            return Ok(CheckFileResult::FilePartialOffset(offset));
        }

        // new file!
        return Ok(CheckFileResult::FilePartialOffset(0));
    }

    pub fn assign_name() {}
}
