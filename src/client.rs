mod proto {
    tonic::include_proto!("raptorboost");
}

use crate::proto::{FileState, UploadFileRequest};

use clap::Parser;
use proto::FileData;
use proto::raptor_boost_client::RaptorBoostClient;
use ring;
use std::fs::File;
use std::io::{self, ErrorKind, Read};
use std::io::{BufReader, Seek, SeekFrom, Write};
use std::process::ExitCode;
use std::time;
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
            println!(
                "resuming {} [{:.2}MB/{:.2}MB]",
                filename,
                offset / 1024 / 1024,
                file_size / 1024 / 1024,
            );
        }

        let mut first = true;
        let freader = BufReader::new(f);

        let mut pos: u64 = offset;
        let mut pos_old = pos;
        let mut percent_old: u32 = 0;
        let mut time_old = time::Instant::now();
        let file_iter = freader.iter_chunks(8192).map(move |d| {
            let percent_cur: u32 = ((pos as f64 / file_size as f64) * 100.0) as u32;
            if percent_cur != percent_old {
                let time_now = time::Instant::now();
                let time_passed = (time_now.duration_since(time_old)).as_millis();
                let mbps = match time_passed {
                    0 => 0,
                    _ => ((pos - pos_old) as u128 / time_passed) / 1000,
                };
                print!(
                    "\r{}% [{}MB/s]",
                    percent_cur,
                    if mbps == 0 {
                        "--".to_string()
                    } else {
                        mbps.to_string()
                    }
                );
                io::stdout().flush().unwrap();
                percent_old = percent_cur;
                time_old = time_now;
                pos_old = pos;
            }
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
                    eprintln!("\rtransfer complete!");
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

    for f in &args.files {
        match send_file(args.host.to_string(), args.port, f.to_string()) {
            Ok(_) => (),
            Err(e) => println!("error sending {}: {}", f, e),
        }
    }

    ExitCode::SUCCESS
}
