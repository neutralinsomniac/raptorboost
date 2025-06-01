include!("service.rs");

use argparse::ArgumentParser;
use proto::raptor_boost_server::{RaptorBoost, RaptorBoostServer};
use rusqlite::{Connection, Result};
use sha2::{Digest, Sha256};
use tonic::{Request, Response, Status, transport::Server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut bind_addr = "[::1]:7272".parse().unwrap();
    let rb_service = RaptorBoostService::default();

    {
        let mut ap = ArgumentParser::new();
        ap.refer(&mut bind_addr)
            .add_option(&["-h", "--host"], argparse::Store, "Bind host addr");

        ap.parse_args_or_exit();
    }

    Server::builder()
        .add_service(RaptorBoostServer::new(rb_service))
        .serve(bind_addr)
        .await?;

    Ok(())
}
