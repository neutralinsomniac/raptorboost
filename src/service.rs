use std::fs::File;
use std::io::{self, Seek, Write};

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
        let mut stream = request.into_inner();

        let Some(first_file_data) = stream.message().await? else {
            return Err(Status::not_found("no data received?"));
        };

        let Some(sha256sum) = first_file_data.sha256sum else {
            return Err(Status::internal("first packet did not contain sha256sum"));
        };

        // check this file's state
        let Ok(file_state) = self.controller.check_file(&sha256sum) else {
            return Err(Status::internal("error checking file state"));
        };

        match file_state {
            controller::CheckFileResult::FileComplete => {
                return Err(Status::already_exists("already exists"));
            }
            controller::CheckFileResult::FilePartialOffset(_) => (),
        }

        // start writing partial file
        let mut partial_path = self.controller.get_partial_dir();
        partial_path.push(&sha256sum);

        let mut f = File::open(&partial_path)?;

        // jump to end
        f.seek(io::SeekFrom::End(0))?;

        // write initial data first
        let total = first_file_data.data.len();
        let mut num_written = 0;

        while num_written < total {
            num_written += f.write(&first_file_data.data)?;
        }

        // now loop over remaining message stream
        while let Some(data) = stream.message().await? {
            let total = data.data.len();
            let mut num_written = 0;
            while num_written < total {
                num_written += f.write(&first_file_data.data)?;
            }
        }

        drop(f);

        let mut complete_path = self.controller.get_complete_dir();
        complete_path.push(&sha256sum);

        std::fs::rename(partial_path, complete_path)?;

        let mut resp = SendFileDataResponse::default();
        resp.status = SendFileDataStatus::SendfiledatastatusComplete.into();

        Ok(Response::new(resp))
    }
}
