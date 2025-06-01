use crate::controller::{self, RaptorBoostError};
use crate::proto::raptor_boost_server::RaptorBoost;
use crate::proto::{
    FileData, FileState, GetVersionRequest, GetVersionResponse, SendFileDataResponse,
    SendFileDataStatus, UploadFileRequest, UploadFileResponse,
};
use tonic::{Request, Response, Status, Streaming};

pub struct RaptorBoostService {
    pub controller: controller::RaptorBoostController,
}

#[tonic::async_trait]
impl RaptorBoost for RaptorBoostService {
    async fn get_version(
        &self,
        _: Request<GetVersionRequest>,
    ) -> Result<Response<GetVersionResponse>, Status> {
        Ok(Response::new(GetVersionResponse {
            version: self.controller.get_version(),
        }))
    }

    async fn upload_file(
        &self,
        request: Request<UploadFileRequest>,
    ) -> Result<Response<UploadFileResponse>, Status> {
        let check_file_result = match self.controller.check_file(&request.into_inner().sha256sum) {
            Ok(r) => r,
            Err(e) => match e {
                RaptorBoostError::PathSanitization => {
                    return Err(Status::invalid_argument(e.to_string()));
                }
                RaptorBoostError::OtherError(e) => return Err(Status::internal(e)),
            },
        };

        match check_file_result {
            controller::CheckFileResult::FileComplete => Ok(Response::new(UploadFileResponse {
                file_state: FileState::FilestateComplete.into(),
                offset: None,
            })),
            controller::CheckFileResult::FilePartialOffset(offset) => {
                Ok(Response::new(UploadFileResponse {
                    file_state: FileState::FilestateNeedMoreData.into(),
                    offset: Some(offset),
                }))
            }
        }
    }

    async fn send_file_data(
        &self,
        request: Request<Streaming<FileData>>,
    ) -> Result<Response<SendFileDataResponse>, Status> {
        Ok(Response::new(SendFileDataResponse {
            status: SendFileDataStatus::SendfiledatastatusComplete.into(),
        }))
    }
}
