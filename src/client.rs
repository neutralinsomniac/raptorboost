mod proto {
    tonic::include_proto!("raptorboost");
}
use proto::FileData;
use proto::raptor_boost_client::RaptorBoostClient;

use crate::proto::{FileState, UploadFileRequest};

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

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(long, short, default_value = "7272")]
    port: u16,
    #[arg(index = 1)]
    host: String,
    #[arg(trailing_var_arg = true, index = 2)]
    files: Vec<String>,
}

fn send_file(host: String, port: u16, filename: String) -> Result<(), Box<dyn std::error::Error>> {
    let rt = Runtime::new()?;
    let mut buffer = [0; 8192];

    let mut f = File::open(&filename)?;
    let file_size = f.metadata()?.len();
    let mut hasher = ring::digest::Context::new(&ring::digest::SHA256);

    println!("calculating checksum for {}...", filename);
    loop {
        match f.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buffer[..n]);
            }
            Err(ref e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => Err(e)?,
        }
    }

    let sha256sum: String = hex::encode(hasher.finish());

    rt.block_on(async {
        let mut client = match RaptorBoostClient::connect(format!("http://{}:{}", host, port)).await
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("error connecting: {}", e);
                return;
            }
        };
        let upload_file_resp = match client
            .upload_file(Request::new(UploadFileRequest {
                sha256sum: sha256sum.to_owned(),
                size: file_size,
            }))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("error uploading file: {}", e);
                return;
            }
        };

        let upload_file_resp = upload_file_resp.into_inner();
        let offset = match upload_file_resp.file_state() {
            FileState::FilestateNeedMoreData => upload_file_resp.offset.unwrap(),
            FileState::FilestateComplete => {
                println!("file already transferred!");
                return;
            }
            _ => {
                eprintln!("how did we get here?");
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
                    let amount_transferred = file_size - offset;
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

fn main() -> ExitCode {
    let args = Args::parse();

    if args.files.len() == 0 {
        eprintln!("no file(s) specified");
        return ExitCode::FAILURE;
    }

    let mut sorted_files = args.files.to_owned();

    sorted_files.sort_by(|a, b| {
        let size_a = File::open(a).unwrap().metadata().unwrap().size();
        let size_b = File::open(b).unwrap().metadata().unwrap().size();
        size_a.cmp(&size_b)
    });

    for f in &sorted_files {
        match send_file(args.host.to_string(), args.port, f.to_string()) {
            Ok(_) => (),
            Err(e) => println!("error sending {}: {}", f, e),
        }
    }

    ExitCode::SUCCESS
}
