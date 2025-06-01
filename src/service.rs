mod proto {
    tonic::include_proto!("raptorboost");
}

use crate::controller;
use crate::proto::raptor_boost_server::RaptorBoost;
use crate::proto::{GetVersionRequest, GetVersionResponse};
use tonic::{Request, Response, Status};

pub struct RaptorBoostService {
    pub controller: Box<controller::RaptorBoostController>,
}

#[tonic::async_trait]
impl RaptorBoost for RaptorBoostService {
    async fn get_version(
        &self,
        _: tonic::Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        Ok(Response::new(GetVersionResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }
}
