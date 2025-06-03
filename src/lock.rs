use std::{fs::OpenOptions, path::PathBuf};

#[derive(Debug)]
pub struct LockFile {
    path: PathBuf,
}

impl LockFile {
    pub fn open(path: PathBuf) -> Result<LockFile, String> {
        if let Err(e) = OpenOptions::new().create_new(true).write(true).open(&path) {
            return Err(format!("couldn't create lock: {}", e));
        };

        Ok(LockFile { path })
    }
}

impl Drop for LockFile {
    fn drop(&mut self) {
        std::fs::remove_file(&self.path).unwrap();
    }
}
