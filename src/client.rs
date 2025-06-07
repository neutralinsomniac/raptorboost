mod proto {
    tonic::include_proto!("raptorboost");
}
use crate::proto::SendFileDataResponse;
use proto::raptor_boost_client::RaptorBoostClient;
use proto::{FileData, FileStateResult};

use crate::proto::{FileState, UploadFilesRequest};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::io::{BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

use clap::Parser;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use ring;
use spat;
use thiserror::Error;
use tokio::runtime::Runtime;
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
            Ok(0) => return None,
            Ok(n) => return Some(Ok(buffer[..n].to_vec())),
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
fn send_file(
    host: &str,
    port: u16,
    file: &FilenameWithState,
    multibar: &mut MultiProgress,
) -> Result<(), SendFileError> {
    let file_size = File::open(&file.filename)
        .map_err(|source| SendFileError::OpenError { source })?
        .metadata()?
        .len();

    let rt = Runtime::new()?;

    let filename = file.filename.to_owned();
    let offset = file.offset;
    let sha256sum = file.sha256sum.to_owned();

    let resp: Result<Response<SendFileDataResponse>, SendFileError> = rt.block_on(async {
        let mut client = RaptorBoostClient::connect(format!("http://{}:{}", host, port)).await?;

        let mut f = File::open(&filename)?;

        f.seek(SeekFrom::Start(offset))
            .map_err(|source| SendFileError::SeekError { source })?;

        // if offset == 0 {
        //     println!("sending {}...", filename);
        // } else {
        //     println!(
        //         "resuming {} from {:.2} MB",
        //         filename,
        //         offset as f64 / 1000.0 / 1000.0
        //     );
        // }

        let mut first = true;
        let freader = BufReader::new(f);

        let mut pos: u64 = offset;
        let bar = ProgressBar::new(file_size - offset).with_style(
            ProgressStyle::with_template(
                "[{elapsed_precise}] \
                 [eta: {eta_precise}] \
                 {wide_bar} \
                 [{decimal_bytes:>7}/{decimal_total_bytes:7}] \
                 [{decimal_bytes_per_sec}]",
            )
            .unwrap(),
        );

        let bar = multibar.add(bar);

        // we fork here to handle the case where a file iterator on an empty (or a partial file with 0 bytes left to transfer) wouldn't iterate
        let resp = if file_size - offset == 0 {
            let mut vec_iter = Vec::new();
            let fdata = FileData {
                sha256sum: Some(sha256sum),
                data: vec![],
            };

            vec_iter.push(fdata);

            let request = Request::new(tokio_stream::iter(vec_iter));
            client.send_file_data(request).await?
        } else {
            let file_iter = freader.iter_chunks(8192).map(move |d| {
                bar.set_position(pos - offset);
                let data = d.unwrap();
                pos += data.len() as u64;
                if first {
                    first = false;
                    FileData {
                        sha256sum: Some(sha256sum.to_string()),
                        data,
                    }
                } else {
                    FileData {
                        sha256sum: None,
                        data,
                    }
                }
            });
            let request = Request::new(tokio_stream::iter(file_iter));
            client.send_file_data(request).await?
        };

        Ok(resp)
    });

    match resp?.into_inner().status() {
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

fn get_file_states(
    host: &str,
    port: u16,
    sha256sums: Vec<String>,
) -> Result<Vec<FileState>, Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;
    rt.block_on(async {
        let mut client = match RaptorBoostClient::connect(format!("http://{}:{}", host, port)).await
        {
            Ok(c) => c,
            Err(e) => {
                return Err(Box::<dyn std::error::Error>::from(format!(
                    "error connecting: {}",
                    e
                )));
            }
        };

        let upload_file_resp = match client
            .upload_files(Request::new(UploadFilesRequest { sha256sums }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Err(Box::<dyn std::error::Error>::from(format!(
                    "error uploading file list: {}",
                    e
                )));
            }
        };

        Ok(upload_file_resp.into_inner().file_states)
    })
}

#[derive(Error, Debug)]
#[error("{0}")]
pub struct MainError(String);

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, short, action, help = "don't sort files by size")]
    no_sort: bool,
    #[arg(long, short, default_value = "7272")]
    port: u16,
    #[arg(index = 1)]
    host: String,
    #[arg(trailing_var_arg = true, index = 2)]
    files: Vec<String>,
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    if args.files.len() == 0 {
        return Err(Box::new(MainError("no file(s) specified".to_string())));
    }

    let mut deduped_filenames = HashSet::new();

    // 1: dedup files
    for f in &args.files {
        let fd = match File::open(f) {
            Ok(fd) => fd,
            Err(e) => return Err(Box::new(MainError(format!("couldn't open '{}': {}", f, e)))),
        };
        if fd.metadata()?.is_dir() {
            for entry in WalkDir::new(f)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| !e.file_type().is_dir())
            {
                let f_name = String::from(entry.path().to_string_lossy());
                deduped_filenames.insert(f_name);
            }
        } else {
            deduped_filenames.insert(f.to_owned());
        }
    }

    if deduped_filenames.len() == 0 {
        return Err(Box::new(MainError("no files found".to_string())));
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
    let mut file_sha256es = HashMap::new();
    let mut sorted_sha256es = Vec::new();
    println!("[+] calculating checksums...");
    let mut multibar = MultiProgress::new();
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
                    return Err(Box::new(MainError(format!("error reading file: {}", e))));
                }
            }
        }

        let sha256sum = hex::encode(hasher.finish());
        file_sha256es.insert(sha256sum.to_owned(), filename);
        sorted_sha256es.push(sha256sum);
        bar.inc(1);
    }

    drop(bar);

    println!("[+] getting file states from remote...");
    // 4: get file states through grpc
    let file_states = match get_file_states(&args.host, args.port, sorted_sha256es) {
        Ok(f) => f,
        Err(e) => {
            return Err(Box::new(MainError(format!(
                "error getting file states: {}",
                e
            ))));
        }
    };

    let mut num_files_up_to_date = 0;
    let mut num_files_sent = 0;
    let mut num_files_to_send = 0;
    for file_state in &file_states {
        match file_state.state() {
            FileStateResult::FilestateresultUnspecified => eprintln!("wut"),
            FileStateResult::FilestateresultNeedMoreData => num_files_to_send += 1,
            FileStateResult::FilestateresultComplete => num_files_up_to_date += 1,
        }
    }

    // ok, we have our filename<->hash mapping and our hash<->filestate mapping, combine them
    let filenames_with_state = file_states.iter().filter_map(|file_state| {
        if file_state.state() == FileStateResult::FilestateresultComplete {
            None
        } else {
            Some(FilenameWithState {
                filename: file_sha256es
                    .get(&file_state.sha256sum)
                    .unwrap()
                    .to_string(),
                sha256sum: file_state.sha256sum.to_owned(),
                offset: file_state.offset(),
            })
        }
    });

    // 5: upload actual file data
    // doing this so we don't have to collect() the above iterator
    if num_files_to_send > 0 {
        println!("[+] sending {} files...", num_files_to_send);
    }
    let mut num_send_errors = 0;
    let total_files_bar = multibar.add(ProgressBar::new(num_files_to_send).with_style(
        ProgressStyle::with_template("[{elapsed_precise}] {wide_bar} {pos:>7}/{len:7}")?,
    ));
    total_files_bar.enable_steady_tick(Duration::new(0, 100000000)); // 10 times per second
    total_files_bar.set_position(0);
    let filename_bar = multibar.add(
        ProgressBar::new(0).with_style(ProgressStyle::with_template("sending {msg}...").unwrap()),
    );

    for f in filenames_with_state {
        let pathbuf = PathBuf::from_str(&f.filename).unwrap();
        let truncated_filename = spat::shorten(pathbuf);
        let truncated_filename = truncated_filename.display().to_string();
        filename_bar.set_message(truncated_filename);

        match send_file(&args.host, args.port, &f, &mut multibar) {
            Ok(_) => {
                num_files_sent += 1;
                total_files_bar.inc(1)
            }
            Err(e) => match e {
                SendFileError::OpenError { source } => {
                    num_send_errors += 1;
                    println!("error opening file '{}': {}", f.filename, source)
                }
                SendFileError::OtherError(error) => {
                    num_send_errors += 1;
                    println!("error occurred with file '{}': {}", f.filename, error)
                }
                SendFileError::ConnectError(error) => {
                    num_send_errors += 1;
                    println!("connection error: {}", error)
                }
                SendFileError::SeekError { source } => {
                    num_send_errors += 1;
                    println!("error occurred with file '{}': {}", f.filename, source)
                }
                SendFileError::ResponseError(status) => {
                    num_send_errors += 1;
                    println!("response error for '{}': {}", f.filename, status)
                }
                SendFileError::ChecksumMismatch => println!("checksum mismatch"),
                SendFileError::UnspecifiedError => println!("unspecified error"),
            },
        }
    }

    drop(filename_bar);
    drop(total_files_bar);

    println!("");

    if num_files_up_to_date != 0 {
        println!("{} files were already up to date", num_files_up_to_date);
    }

    println!("{} files were sent", num_files_sent);

    if num_send_errors > 0 {
        println!("couldn't send {} files due to errors", num_send_errors)
    }

    Ok(())
}
