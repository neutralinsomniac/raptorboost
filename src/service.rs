use std::collections::HashSet;

use crate::controller::{self, RaptorBoostError};
use crate::proto::raptor_boost_server::RaptorBoost;
use crate::proto::{
    AssignNameRequest, AssignNameResponse, AssignNameStatus, FileData, FileState, FileStateResult,
    GetVersionRequest, GetVersionResponse, SendFileDataResponse, SendFileDataStatus,
    UploadFilesRequest, UploadFilesResponse,
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

    async fn upload_files(
        &self,
        request: Request<UploadFilesRequest>,
    ) -> Result<Response<UploadFilesResponse>, Status> {
        let mut seen_sha256es = HashSet::new();

        let file_states: Result<Vec<FileState>, _> = request
            .into_inner()
            .sha256sums
            .iter()
            .filter_map(|sha256sum| {
                if seen_sha256es.contains(sha256sum) {
                    return None;
                }

                seen_sha256es.insert(sha256sum.to_owned());

                let check_file_result = match self.controller.check_file(&sha256sum) {
                    Ok(r) => r,
                    Err(e) => match e {
                        RaptorBoostError::PathSanitization => {
                            return Some(Err(Status::invalid_argument(e.to_string())));
                        }
                        RaptorBoostError::OtherError(e) => return Some(Err(Status::internal(e))),
                        RaptorBoostError::LockFailure => {
                            return Some(Err(Status::unavailable("couldn't lock!")));
                        }
                        _ => todo!("sort out these extra errors"),
                    },
                };

                match check_file_result {
                    controller::CheckFileResult::FileComplete => Some(Ok(FileState {
                        sha256sum: sha256sum.to_owned(),
                        state: FileStateResult::FilestateresultComplete.into(),
                        offset: None,
                    })),
                    controller::CheckFileResult::FilePartialOffset(offset) => Some(Ok(FileState {
                        sha256sum: sha256sum.to_owned(),
                        state: FileStateResult::FilestateresultNeedMoreData.into(),
                        offset: Some(offset),
                    })),
                }
            })
            .collect();

        match file_states {
            Ok(states) => Ok(Response::new(UploadFilesResponse {
                file_states: states,
            })),
            Err(e) => Err(Status::internal(e.to_string())),
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
        while let Ok(message) = stream.message().await {
            match message {
                Some(data) => {
                    let total = data.data.len();
                    let mut num_written = 0;
                    while num_written < total {
                        num_written += transfer_object.write(&data.data)?;
                    }
                }
                None => break,
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

    async fn assign_name(
        &self,
        request: Request<AssignNameRequest>,
    ) -> Result<Response<AssignNameResponse>, Status> {
        Ok(Response::new(AssignNameResponse {
            status: AssignNameStatus::AssignnamestatusSuccess.into(),
        }))
    }
}
