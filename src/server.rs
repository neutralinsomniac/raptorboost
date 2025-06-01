mod proto {
    tonic::include_proto!("raptorboost");
}

mod controller;
mod db;
mod service;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use proto::raptor_boost_server::RaptorBoostServer;
use rusqlite::Result;
use tonic::transport::Server;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[arg(short, long, default_value = "[::1]:7272")]
    bind_addr: SocketAddr,
    #[arg(short, long, default_value = std::env::current_dir().unwrap().into_os_string())]
    out_dir: PathBuf,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    // let bind_addr = args.bind_addr.parse().unwrap();

    println!("{}", args.out_dir.display());
    let controller = controller::RaptorBoostController::new(&args.out_dir)?;
    let rb_service = service::RaptorBoostService { controller };

    Server::builder()
        .add_service(RaptorBoostServer::new(rb_service))
        .serve(args.bind_addr)
        .await?;

    Ok(())
}
