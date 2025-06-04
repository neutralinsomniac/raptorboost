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
                RaptorBoostError::LockFailure => return Err(Status::unavailable("couldn't lock!")),
                RaptorBoostError::TransferAlreadyComplete => {
                    return Err(Status::already_exists("transfer already exists!"));
                }
                _ => todo!("sort out these extra errors"),
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
        let mut stream = request.into_inner();

        let Some(first_file_data) = stream.message().await? else {
            return Err(Status::not_found("no data received?"));
        };

        let Some(sha256sum) = first_file_data.sha256sum else {
            return Err(Status::internal("first packet did not contain sha256sum"));
        };

        let mut transfer_object = match self.controller.start_transfer(&sha256sum) {
            Ok(t) => t,
            Err(e) => match e {
                RaptorBoostError::LockFailure => return Err(Status::unavailable("couldn't lock!")),
                RaptorBoostError::PathSanitization => {
                    return Err(Status::invalid_argument("bad argument"));
                }
                RaptorBoostError::OtherError(e) => return Err(Status::internal(e)),
                RaptorBoostError::TransferAlreadyComplete => {
                    return Err(Status::already_exists("already exists"));
                }
                _ => return Err(Status::internal("unexpected error occurred")),
            },
        };

        // write initial data first
        let total = first_file_data.data.len();
        let mut num_written = 0;

        while num_written < total {
            num_written += transfer_object.write(&first_file_data.data)?;
        }

        // now loop over remaining message stream
        while let Some(data) = stream.message().await? {
            let total = data.data.len();
            let mut num_written = 0;
            while num_written < total {
                num_written += transfer_object.write(&data.data)?;
            }
        }

        let mut resp = SendFileDataResponse::default();

        match transfer_object.complete() {
            Ok(_) => {
                resp.status = SendFileDataStatus::SendfiledatastatusComplete.into();
                return Ok(Response::new(resp));
            }
            Err(e) => match e {
                RaptorBoostError::RenameError(e) => {
                    return Err(Status::internal(e));
                }
                RaptorBoostError::ChecksumMismatch => {
                    return Err(Status::data_loss("checksum mismatch!"));
                }
                _ => return Err(Status::internal("unexpected error occurred")),
            },
        }
    }
}
