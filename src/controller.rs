use std::{
    error::Error,
    fs::{self, File},
    io::{Seek, SeekFrom},
    path::PathBuf,
};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum RaptorBoostError {
    #[error("path is not clean")]
    PathSanitization,
    #[error("other error: `{0}`")]
    OtherError(String),
}

#[derive(Error, Debug)]
#[error("{0}")]
pub struct RaptorBoostControllerError(String);

pub struct RaptorBoostController {
    partial_dir: PathBuf,
    complete_dir: PathBuf,
}

pub enum CheckFileResult {
    FileComplete,
    FilePartialOffset(u64),
}

impl RaptorBoostController {
    pub fn new(base_dir: &PathBuf) -> Result<RaptorBoostController, Box<dyn Error>> {
        // base dir must exist
        if !base_dir.try_exists()? {
            return Err(Box::new(RaptorBoostControllerError(
                "base directory doesn't exist".to_string(),
            )));
        }

        // ensure directories exist
        let mut partial_dir = base_dir.to_owned();
        partial_dir.push("partial");

        if !partial_dir.exists() {
            match fs::create_dir(&partial_dir) {
                Ok(_) => (),
                Err(e) => return Err(Box::new(e)),
            }
        }

        let mut complete_dir = base_dir.to_owned();
        complete_dir.push("complete");

        if !complete_dir.exists() {
            match fs::create_dir(&complete_dir) {
                Ok(_) => (),
                Err(e) => return Err(Box::new(e)),
            }
        }

        Ok(RaptorBoostController {
            partial_dir,
            complete_dir,
        })
    }

    pub fn get_partial_dir(&self) -> PathBuf {
        return self.partial_dir.to_owned();
    }

    pub fn get_complete_dir(&self) -> PathBuf {
        return self.complete_dir.to_owned();
    }

    pub fn get_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    pub fn check_file(&self, sha256sum: &str) -> Result<CheckFileResult, RaptorBoostError> {
        // first look for file in complete
        let mut full_complete_file = self.get_complete_dir();
        full_complete_file.push(sha256sum);

        match full_complete_file.parent() {
            Some(p) => {
                if p != self.complete_dir {
                    return Err(RaptorBoostError::PathSanitization);
                }
            }
            None => {
                return Err(RaptorBoostError::PathSanitization);
            }
        }

        if full_complete_file.exists() {
            return Ok(CheckFileResult::FileComplete);
        }

        let mut full_partial_file = self.get_partial_dir();
        full_partial_file.push(sha256sum);

        match full_partial_file.parent() {
            Some(p) => {
                if p != self.partial_dir {
                    return Err(RaptorBoostError::PathSanitization);
                }
            }
            None => {
                return Err(RaptorBoostError::PathSanitization);
            }
        }

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

        return Ok(CheckFileResult::FilePartialOffset(0));
    }
}
