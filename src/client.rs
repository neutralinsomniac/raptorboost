mod proto {
    tonic::include_proto!("raptorboost");
}
use crate::proto::SendFileDataResponse;
use proto::raptor_boost_client::RaptorBoostClient;
use proto::{AssignNamesRequest, FileData, FileStateResult, Sha256Filenames};

use crate::proto::{FileState, UploadFilesRequest};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::io::{BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use std::sync::mpsc;
use thiserror::Error;
use tonic::{Request, Response};
use walkdir::WalkDir;

pub struct ToChunks<R> {
    reader: R,
    chunk_size: usize,
}

impl<R: Read> Iterator for ToChunks<R> {
    type Item = io::Result<Vec<u8>>;

    fn next(&mut self) -> Option<Self::Item> {
        let mut buffer = vec![0u8; self.chunk_size];
        match self.reader.read(&mut buffer) {
            Ok(0) => None,
            Ok(n) => Some(Ok(buffer[..n].to_vec())),
            Err(e) => Some(Err(e)),
        }
    }
}

pub trait IterChunks {
    type Output;

    fn iter_chunks(self, len: usize) -> Self::Output;
}

impl<R: Read> IterChunks for R {
    type Output = ToChunks<R>;

    fn iter_chunks(self, len: usize) -> Self::Output {
        ToChunks {
            reader: self,
            chunk_size: len,
        }
    }
}

struct FilenameWithState {
    filename: String,
    sha256sum: String,
    offset: u64,
}

#[derive(Error, Debug)]
enum SendFileError {
    #[error(transparent)]
    ConnectError(#[from] tonic::transport::Error),
    #[error("open error")]
    OpenError { source: std::io::Error },
    #[error("seek error")]
    SeekError { source: std::io::Error },
    #[error(transparent)]
    ResponseError(#[from] tonic::Status),
    #[error("checksum mismatch")]
    ChecksumMismatch,
    #[error(transparent)]
    OtherError(#[from] std::io::Error),
    #[error("unspecified error")]
    UnspecifiedError,
}

async fn send_files(
    host: &str,
    port: u16,
    files: Vec<FilenameWithState>,
    force_unlock: bool,
    multibar: MultiProgress,
) -> Result<(), SendFileError> {
    let mut client = RaptorBoostClient::connect(format!("http://{}:{}", host, port)).await?;

    let filename_bar = multibar.add(
        ProgressBar::new(0).with_style(ProgressStyle::with_template("sending {msg}...").unwrap()),
    );

    let (tx, rx) = mpsc::sync_channel::<FileData>(1);

    let mut total_file_size: u64 = 0;
    for f in &files {
        let len = std::fs::metadata(&f.filename)
            .map_err(|source| SendFileError::OpenError { source })?
            .len();
        total_file_size += len - f.offset;
    }

    let total_file_size_bar = multibar.add(
        ProgressBar::new(total_file_size).with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] \
                 [eta: {eta_precise}] \
                 {wide_bar} \
                 [{decimal_bytes:>7}/{decimal_total_bytes:7}] \
                 [{decimal_bytes_per_sec}]",
            )
            .unwrap(),
        ),
    );

    let send_task: tokio::task::JoinHandle<Result<(), SendFileError>> =
        tokio::spawn(async move {
            for file in files {
                let file_size = std::fs::metadata(&file.filename)
                    .map_err(|source| SendFileError::OpenError { source })?
                    .len();

                let mut f = File::open(&file.filename)
                    .map_err(|source| SendFileError::OpenError { source })?;
                f.seek(SeekFrom::Start(file.offset))
                    .map_err(|source| SendFileError::SeekError { source })?;

                let freader = BufReader::new(f);

                let truncated_filename =
                    spat::shorten(PathBuf::from_str(&file.filename).unwrap())
                        .display()
                        .to_string();
                filename_bar.set_message(truncated_filename);

                // empty file (or partial with 0 bytes left): send a single empty frame
                if file_size - file.offset == 0 {
                    let fdata = FileData {
                        first: true,
                        last: true,
                        sha256sum: Some(file.sha256sum),
                        force: Some(force_unlock),
                        data: vec![],
                    };
                    if tx.send(fdata).is_err() {
                        return Ok(());
                    }
                    continue;
                }

                let mut first = true;
                let mut pos: u64 = file.offset;

                for d in freader.iter_chunks(8192) {
                    let data = d?;
                    pos += data.len() as u64;
                    total_file_size_bar.inc(data.len() as u64);
                    let fdata = if first {
                        first = false;
                        FileData {
                            first: true,
                            last: file_size == pos,
                            sha256sum: Some(file.sha256sum.clone()),
                            force: Some(force_unlock),
                            data,
                        }
                    } else {
                        FileData {
                            first: false,
                            last: file_size == pos,
                            sha256sum: None,
                            force: None,
                            data,
                        }
                    };
                    if tx.send(fdata).is_err() {
                        return Ok(());
                    }
                }
            }
            Ok(())
        });

    let request = Request::new(tokio_stream::iter(rx));
    let resp: Result<Response<SendFileDataResponse>, tonic::Status> =
        client.send_file_data(request).await;

    // surface any producer-side error
    if let Ok(Err(e)) = send_task.await {
        return Err(e);
    }

    let resp = match resp {
        Err(e) => {
            eprintln!("err: {}", e);
            return Err(SendFileError::UnspecifiedError);
        }
        Ok(r) => r,
    };

    match resp.into_inner().status() {
        proto::SendFileDataStatus::SendfiledatastatusUnspecified => {
            eprintln!("\runspecified error occurred");
            Err(SendFileError::UnspecifiedError)
        }
        proto::SendFileDataStatus::SendfiledatastatusComplete => Ok(()),
        proto::SendFileDataStatus::SendfiledatastatusErrorChecksum => {
            eprintln!("\rchecksum error!");
            Err(SendFileError::ChecksumMismatch)
        }
    }
}

async fn get_file_states(
    host: &str,
    port: u16,
    sha256sums: Vec<String>,
) -> Result<Vec<FileState>, Box<dyn std::error::Error>> {
    let mut client = RaptorBoostClient::connect(format!("http://{}:{}", host, port))
        .await
        .map_err(|e| format!("error connecting: {}", e))?;

    let upload_file_resp = client
        .upload_files(Request::new(UploadFilesRequest { sha256sums }))
        .await
        .map_err(|e| format!("error uploading file list: {}", e))?;

    Ok(upload_file_resp.into_inner().file_states)
}

#[derive(Error, Debug)]
#[error("{0}")]
pub struct MainError(String);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, short, default_value = "7272")]
    port: u16,
    #[arg(short, long)]
    name: Option<String>,
    #[arg(long, action, help = "don't sort files by size")]
    no_sort: bool,
    #[arg(long, action)]
    force_unlock: bool,
    #[arg(long, action, default_value = "false")]
    force_name: bool,
    #[arg(index = 1)]
    host: String,
    #[arg(trailing_var_arg = true, index = 2)]
    files: Vec<String>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.files.is_empty() {
        return Err(MainError("no file(s) specified".to_string()).into());
    }

    let mut deduped_filenames: HashSet<String> = HashSet::new();

    // 1: dedup files
    for f in &args.files {
        let fd = match File::open(f) {
            Ok(fd) => fd,
            Err(e) => return Err(MainError(format!("couldn't open '{}': {}", f, e)).into()),
        };
        if fd.metadata()?.is_dir() {
            for entry in WalkDir::new(f)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| !e.file_type().is_dir() && !e.file_type().is_symlink())
            {
                deduped_filenames.insert(entry.path().to_string_lossy().into_owned());
            }
        } else {
            deduped_filenames.insert(f.to_owned());
        }
    }

    if deduped_filenames.is_empty() {
        return Err(MainError("no files found".to_string()).into());
    }

    // 2: sort files
    let mut sorted_files: Vec<&String> = deduped_filenames.iter().collect();

    if !args.no_sort {
        println!("[+] sorting files...");
        sorted_files.sort_by(|a, b| {
            let size_a = File::open(a).unwrap().metadata().unwrap().size();
            let size_b = File::open(b).unwrap().metadata().unwrap().size();
            size_b.cmp(&size_a)
        })
    }

    // 3: calculate checksums
    let mut filename_to_sha256es = HashMap::new();
    let mut sha256_to_filenames: HashMap<String, Vec<&String>> = HashMap::new();
    let mut sorted_sha256es = Vec::new();
    println!("[+] calculating checksums...");
    let multibar = MultiProgress::new();
    let bar = multibar.add(ProgressBar::new(sorted_files.len().try_into().unwrap()));
    for filename in sorted_files {
        bar.tick(); // show the bar even if the first file takes a while to checksum

        let mut f = File::open(filename).unwrap();

        let mut buffer = [0; 8192];

        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);

        loop {
            match f.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    return Err(MainError(format!("error reading `{}`: {}", filename, e)).into());
                }
            }
        }

        let sha256sum = hex::encode(hasher.finish());
        filename_to_sha256es.insert(sha256sum.clone(), filename);
        sorted_sha256es.push(sha256sum.clone());
        sha256_to_filenames
            .entry(sha256sum)
            .or_default()
            .push(filename);
        bar.inc(1);
    }

    drop(bar);

    println!("[+] getting file states from remote...");
    // 4: get file states through grpc
    let file_states = get_file_states(&args.host, args.port, sorted_sha256es)
        .await
        .map_err(|e| MainError(format!("error getting file states: {}", e)))?;

    let mut num_files_up_to_date = 0;
    let mut num_files_to_send = 0;
    for file_state in &file_states {
        match file_state.state() {
            FileStateResult::FilestateresultUnspecified => eprintln!("wut"),
            FileStateResult::FilestateresultNeedMoreData => num_files_to_send += 1,
            FileStateResult::FilestateresultComplete => num_files_up_to_date += 1,
        }
    }

    // ok, we have our filename<->hash mapping and our hash<->filestate mapping, combine them
    let filenames_with_state: Vec<FilenameWithState> = file_states
        .iter()
        .filter_map(|file_state| {
            if file_state.state() == FileStateResult::FilestateresultComplete {
                None
            } else {
                Some(FilenameWithState {
                    filename: filename_to_sha256es
                        .get(&file_state.sha256sum)
                        .unwrap()
                        .to_string(),
                    sha256sum: file_state.sha256sum.to_owned(),
                    offset: file_state.offset(),
                })
            }
        })
        .collect();

    // 5: upload actual file data
    if num_files_to_send > 0 {
        println!("[+] sending {} files...", num_files_to_send);
    }

    send_files(
        &args.host,
        args.port,
        filenames_with_state,
        args.force_unlock,
        multibar,
    )
    .await?;

    // 6: send names
    println!("[+] updating filenames...");
    let mut client =
        RaptorBoostClient::connect(format!("http://{}:{}", args.host, args.port)).await?;

    let assign_names_resp = client
        .assign_names(Request::new(AssignNamesRequest {
            sha256_to_filenames: sha256_to_filenames
                .iter()
                .map(|(sha256sum, filenames)| Sha256Filenames {
                    sha256sum: sha256sum.to_string(),
                    names: filenames.iter().map(|&name| name.to_string()).collect(),
                })
                .collect(),
            name: args.name,
            force: args.force_name.then_some(true),
        }))
        .await;

    if let Err(e) = assign_names_resp {
        println!("remote error assigning names: {}", e.message());
    }

    println!();

    if num_files_up_to_date != 0 {
        println!("{} files were already up to date", num_files_up_to_date);
    }

    Ok(())
}
