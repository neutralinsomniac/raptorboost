include!("service.rs");

use std::env;

use proto::raptor_boost_server::{RaptorBoost, RaptorBoostServer};
use rusqlite::{Connection, Result};
use sha2::{Digest, Sha256};
use tonic::{Request, Response, Status, transport::Server};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}
