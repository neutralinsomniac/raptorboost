pub mod proto {
    tonic::include_proto!("raptorboost");
}

use proto::{GetVersionRequest, GetVersionResponse};

#[derive(Debug, Default)]
pub struct RaptorBoostService {}

#[tonic::async_trait]
impl RaptorBoost for RaptorBoostService {
    async fn get_version(
        &self,
        _: Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        Ok(Response::new(proto::GetVersionResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }))
    }
}
