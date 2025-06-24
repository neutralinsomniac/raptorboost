use std::collections::HashSet;
use std::fs::{create_dir, create_dir_all, remove_dir_all};
use std::os::unix::fs::symlink;
use std::path::Path;

use crate::controller::{self, RaptorBoostError, RaptorBoostTransfer};
use crate::proto::raptor_boost_server::RaptorBoost;
use crate::proto::{
    AssignNamesRequest, AssignNamesResponse, FileData, FileState, FileStateResult,
    GetVersionRequest, GetVersionResponse, SendFileDataResponse, SendFileDataStatus,
    UploadFilesRequest, UploadFilesResponse,
};

use chrono::Local;
use safe_path::{scoped_join, scoped_resolve};
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
                // we've already handled this result; filter it out from our results
                if seen_sha256es.contains(sha256sum) {
                    return None;
                }

                seen_sha256es.insert(sha256sum.to_owned());

                let check_file_result = match self.controller.check_file(&sha256sum) {
                    Ok(r) => r,
                    Err(e) => match e {
                        RaptorBoostError::PathSanitization(e) => {
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

        let mut transfer_object: RaptorBoostTransfer;

        'next_file: loop {
            let Some(file_data) = stream.message().await? else {
                return Ok(Response::new(SendFileDataResponse {
                    status: SendFileDataStatus::SendfiledatastatusComplete.into(),
                }));
            };

            if file_data.first {
                // verify sha256sum exists
                let Some(sha256sum) = file_data.sha256sum else {
                    return Err(Status::invalid_argument(
                        "need sha256sum in first data packet",
                    ));
                };

                let force = match file_data.force {
                    Some(t) => t,
                    None => false,
                };

                transfer_object = match self.controller.start_transfer(&sha256sum, force) {
                    Ok(t) => t,
                    Err(e) => match e {
                        RaptorBoostError::LockFailure => {
                            return Err(Status::unavailable("couldn't lock!"));
                        }
                        RaptorBoostError::PathSanitization(e) => {
                            return Err(Status::invalid_argument(e.to_string()));
                        }
                        RaptorBoostError::OtherError(e) => return Err(Status::internal(e)),
                        RaptorBoostError::TransferAlreadyComplete => {
                            return Err(Status::already_exists("already exists"));
                        }
                        _ => return Err(Status::internal("unexpected error occurred")),
                    },
                }
            } else {
                return Err(Status::invalid_argument("first packet not marked as first"));
            }

            // write this first file chunk
            let total = file_data.data.len();
            let mut num_written = 0;

            while num_written < total {
                num_written += transfer_object.write(&file_data.data[num_written..])?;
            }

            if file_data.last {
                match transfer_object.complete() {
                    Ok(_) => (),
                    Err(e) => println!("error: {}", e.to_string()),
                }
                continue;
            }

            // now loop over remaining message stream
            while let Some(file_data) = stream.message().await? {
                let total = file_data.data.len();
                let mut num_written = 0;

                while num_written < total {
                    num_written += transfer_object.write(&file_data.data[num_written..])?;
                }

                if file_data.last {
                    match transfer_object.complete() {
                        Ok(_) => (),
                        Err(e) => println!("error: {}", e.to_string()),
                    }
                    continue 'next_file;
                }
            }
        }
    }

    async fn assign_names(
        &self,
        request: Request<AssignNamesRequest>,
    ) -> Result<Response<AssignNamesResponse>, Status> {
        // convenience
        let now = Local::now();

        let assign_name_request = request.into_inner();

        let transfer_dir = scoped_join(
            self.controller.get_transfers_dir(),
            match assign_name_request.name {
                None => format!("{}", now.format("%Y-%m-%d_%H:%M:%S")),
                Some(ref name) => name.to_string(),
            },
        )?;

        if assign_name_request.force() {
            let _ = remove_dir_all(&transfer_dir);
        }

        match create_dir(&transfer_dir) {
            Ok(_) => (),
            Err(e) => {
                return Err(Status::invalid_argument(format!(
                    "couldn't create transfer directory: {}",
                    e
                )));
            }
        }

        let complete_dir = self.controller.get_complete_dir();

        for sha256tonames in assign_name_request.sha256_to_filenames {
            for name in sha256tonames.names {
                let mut path = Path::new(&name);

                // strip leading "/"
                if path.has_root() {
                    path = path.strip_prefix("/").unwrap();
                }

                // strip leading ..'s
                while path.starts_with("..") {
                    path = path.strip_prefix("..").unwrap();
                }

                // split into path + directory component
                let dir = path.parent().unwrap();
                let file = path.file_name().unwrap();

                let _ =
                    create_dir_all(&transfer_dir.join(scoped_resolve(&transfer_dir, dir).unwrap()));

                let safe_target_sha256sum = &complete_dir
                    .join(scoped_resolve(&complete_dir, &sha256tonames.sha256sum).unwrap());

                let safe_target_link_dir =
                    &transfer_dir.join(scoped_resolve(&transfer_dir, dir).unwrap());
                let safe_target_link =
                    &safe_target_link_dir.join(scoped_resolve(safe_target_link_dir, file).unwrap());

                symlink(safe_target_sha256sum, safe_target_link).unwrap();
            }
        }
        Ok(Response::new(AssignNamesResponse { statuses: vec![] }))
    }
}
