mod proto {
    tonic::include_proto!("raptorboost");
}
use proto::raptor_boost_client::RaptorBoostClient;
use proto::{FileData, FileStateResult};

use crate::proto::{FileState, UploadFilesRequest};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::io::{BufReader, Seek, SeekFrom};
use std::os::unix::fs::MetadataExt;
use std::process::ExitCode;
use std::time;

use clap::Parser;
use indicatif::{ProgressBar, ProgressStyle};
use ring;
use tokio::runtime::Runtime;
use tonic::Request;
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

fn send_file(
    host: &str,
    port: u16,
    file: &FilenameWithState,
) -> Result<(), Box<dyn std::error::Error>> {
    let f = File::open(&file.filename)?;
    let file_size = f.metadata()?.len();

    if file_size == 0 {
        return Err(Box::<dyn std::error::Error>::from(
            "skipping file since it's empty",
        ));
    }

    let rt = Runtime::new()?;

    let filename = file.filename.to_owned();
    let offset = file.offset;
    let sha256sum = file.sha256sum.to_owned();

    rt.block_on(async {
        let mut client = match RaptorBoostClient::connect(format!("http://{}:{}", host, port)).await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error connecting: {}", e);
                return;
            }
        };

        let mut f = File::open(&filename).unwrap();
        match f.seek(SeekFrom::Start(offset)) {
            Ok(_) => (),
            Err(e) => {
                eprintln!("error seeking: {}", e);
                return;
            }
        }

        if offset == 0 {
            println!("sending {}...", filename);
        } else {
            println!( "resuming {}", filename);
        }

        let mut first = true;
        let freader = BufReader::new(f);

        let mut pos: u64 = offset;
        let time_start = time::Instant::now();
        let bar = ProgressBar::new(file_size-offset).with_style(
            ProgressStyle::with_template(
                "{msg}[{elapsed_precise}] [eta: {eta_precise}] {bar:40} [{decimal_bytes:>7}/{decimal_total_bytes:7}] [{decimal_bytes_per_sec}]",
            )
            .unwrap(),
        );
        let file_iter = freader.iter_chunks(8192).map(move |d| {
            bar.set_position(pos-offset);
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

        match client.send_file_data(request).await {
            Ok(r) => match r.into_inner().status() {
                proto::SendFileDataStatus::SendfiledatastatusUnspecified => {
                    eprintln!("\runspecified error occurred");
                }
                proto::SendFileDataStatus::SendfiledatastatusComplete => {
                    let duration = time_start.elapsed().as_millis();
                    let amount_transferred = file_size - file.offset;
                    eprintln!(
                        "\rtransferred {:.2}MB in {:.2}s ({}MB/s)",
                        amount_transferred as f64 / 1024.0 / 1024.0,
                        duration as f64 / 1000.0,
                        if duration != 0 {
                            format!(
                                "{:.2}",
                                ((amount_transferred as f64 / 1024.0 / 1024.0)
                                    / (duration as f64 / 1000.0))
                            )
                        } else {
                            "--".to_string()
                        },
                    );
                }

                proto::SendFileDataStatus::SendfiledatastatusErrorChecksum => {
                    eprintln!("\rchecksum error!");
                }
            },
            Err(e) => eprintln!("\rerror streaming: {}", e),
        };
    });

    Ok(())
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
                eprintln!("error connecting: {}", e);
                return Err(Box::<dyn std::error::Error>::from("arst"));
            }
        };

        let upload_file_resp = match client
            .upload_files(Request::new(UploadFilesRequest { sha256sums }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error uploading file list: {}", e);
                return Err(Box::<dyn std::error::Error>::from("arst"));
            }
        };

        Ok(upload_file_resp.into_inner().file_states)
    })
}

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
fn main() -> ExitCode {
    let args = Args::parse();

    if args.files.len() == 0 {
        eprintln!("no file(s) specified");
        return ExitCode::FAILURE;
    }

    let mut deduped_filenames = HashSet::new();

    for f in &args.files {
        if File::open(f).unwrap().metadata().unwrap().is_dir() {
            for entry in WalkDir::new(f)
                .into_iter()
                .filter_map(Result::ok)
                .filter(|e| !e.file_type().is_dir())
            {
                let f_name = String::from(entry.path().to_string_lossy());
                deduped_filenames.insert(f_name.clone());
            }
        } else {
            deduped_filenames.insert(f.to_string());
        }
    }

    let mut file_sha256es = HashMap::new();

    let mut sorted_files: Vec<&String> = deduped_filenames.iter().collect();

    if !args.no_sort {
        sorted_files.sort_by(|a, b| {
            let size_a = File::open(a).unwrap().metadata().unwrap().size();
            let size_b = File::open(b).unwrap().metadata().unwrap().size();
            size_a.cmp(&size_b)
        })
    }

    let mut sorted_sha256es = Vec::new();

    println!("calculating checksums...");
    let bar = ProgressBar::new(sorted_files.len().try_into().unwrap());
    for filename in sorted_files {
        bar.tick(); // show the bar even if the first file takes a while to checksum

        let mut f = File::open(filename).unwrap();

        let mut buffer = [0; 8192];

        let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);

        // println!("calculating checksum for {}...", filename);
        loop {
            match f.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    hasher.update(&buffer[..n]);
                }
                Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => {
                    eprintln!("error reading file: {}", e);
                    return ExitCode::FAILURE;
                }
            }
        }

        let sha256sum = hex::encode(hasher.finish());
        file_sha256es.insert(sha256sum.to_owned(), filename.to_owned());
        sorted_sha256es.push(sha256sum.to_owned());
        bar.inc(1);
    }

    drop(bar);

    let file_states = match get_file_states(&args.host, args.port, sorted_sha256es) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error getting file states: {}", e);
            return ExitCode::FAILURE;
        }
    };

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

    // doing this so we don't have to collect() the above iterator
    let mut send_at_least_one_file = false;
    for f in filenames_with_state {
        send_at_least_one_file = true;
        match send_file(&args.host, args.port, &f) {
            Ok(_) => (),
            Err(e) => println!("error sending {}: {}", f.filename, e),
        }
    }

    if send_at_least_one_file == false {
        println!("all files already transferred!")
    }

    ExitCode::SUCCESS
}
