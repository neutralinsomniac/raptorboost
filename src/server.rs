mod proto {
    tonic::include_proto!("raptorboost");
}

mod controller;
mod lock;
mod service;

use std::path::PathBuf;
use std::str::FromStr;
use std::{net::SocketAddr, process::ExitCode};

use clap::{ArgAction, Parser};
use local_ip_address::list_afinet_netifas;
use proto::raptor_boost_server::RaptorBoostServer;
use tonic::transport::Server;

#[derive(Parser)]
#[command(version, about, disable_help_flag = true)]
struct Args {
    #[arg(short, long, default_value = "127.0.0.1")]
    host: String,
    #[arg(short, long)]
    interface: Option<String>,
    #[arg(short, long, default_value = "7272")]
    port: u16,
    #[arg(short, long, default_value = std::env::current_dir().unwrap().into_os_string())]
    out_dir: PathBuf,
    #[arg(long, action=ArgAction::Help)]
    help: Option<bool>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = Args::parse();

    let controller = match controller::RaptorBoostController::new(&args.out_dir) {
        Ok(c) => c,
        Err(e) => {
            println!("couldn't create controller: {}", e);
            return ExitCode::FAILURE;
        }
    };

    let rb_service = service::RaptorBoostService { controller };

    let mut host = args.host;

    if let Some(interface) = args.interface {
        let mut found_intf = false;
        match list_afinet_netifas() {
            Ok(interfaces) => {
                for (name, ip) in interfaces {
                    if name == interface {
                        host = ip.to_string();
                        found_intf = true;
                        break;
                    }
                }
            }
            Err(e) => {
                println!("couldn't get list of local interfaces: {}", e);
                return ExitCode::FAILURE;
            }
        }
        if !found_intf {
            eprintln!("couldn't find interface {}", interface);
            return ExitCode::FAILURE;
        }
    }

    let bind_addr = match SocketAddr::from_str(&format!("{}:{}", &host, &args.port)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("couldn't parse host/port: {}", e);
            return ExitCode::FAILURE;
        }
    };

    match Server::builder()
        .add_service(RaptorBoostServer::new(rb_service))
        .serve(bind_addr)
        .await
    {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error from grpc server: {}", e);
            return ExitCode::FAILURE;
        }
    }
}
