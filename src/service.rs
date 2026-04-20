use std::collections::HashSet;
use std::fs::{create_dir, create_dir_all, remove_dir_all};
use std::os::unix::fs::symlink;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use crate::controller::{self, RaptorBoostError, RaptorBoostTransfer};
use crate::proto::raptor_boost_server::RaptorBoost;
use crate::proto::{
    AssignNamesRequest, AssignNamesResponse, FileData, FileState, FileStateResult,
    GetVersionRequest, GetVersionResponse, SendFileDataResponse, SendFileDataStatus,
    Sha256Filenames, UploadFilesRequest, UploadFilesResponse,
};

use chrono::Local;
use safe_path::{scoped_join, scoped_resolve};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

pub struct RaptorBoostService {
    pub controller: Arc<controller::RaptorBoostController>,
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

    type UploadFilesStream =
        Pin<Box<dyn Stream<Item = Result<UploadFilesResponse, Status>> + Send + 'static>>;

    async fn upload_files(
        &self,
        request: Request<Streaming<UploadFilesRequest>>,
    ) -> Result<Response<Self::UploadFilesStream>, Status> {
        let stream = request.into_inner();
        let controller = self.controller.clone();
        let mut seen: HashSet<String> = HashSet::new();

        let out = stream.map(move |req_result| -> Result<UploadFilesResponse, Status> {
            let req = req_result?;
            let mut states = Vec::with_capacity(req.sha256sums.len());

            for sha256sum in req.sha256sums {
                if !seen.insert(sha256sum.clone()) {
                    continue;
                }
                match controller.check_file(&sha256sum) {
                    Ok(controller::CheckFileResult::FileComplete) => states.push(FileState {
                        sha256sum,
                        state: FileStateResult::FilestateresultComplete.into(),
                        offset: None,
                    }),
                    Ok(controller::CheckFileResult::FilePartialOffset(offset)) => {
                        states.push(FileState {
                            sha256sum,
                            state: FileStateResult::FilestateresultNeedMoreData.into(),
                            offset: Some(offset),
                        })
                    }
                    Err(e) => {
                        return Err(match e {
                            RaptorBoostError::PathSanitization(msg) => {
                                Status::invalid_argument(msg)
                            }
                            RaptorBoostError::OtherError(msg) => Status::internal(msg),
                            RaptorBoostError::LockFailure => {
                                Status::unavailable("couldn't lock!")
                            }
                            _ => Status::internal("unexpected error"),
                        });
                    }
                }
            }

            Ok(UploadFilesResponse {
                file_states: states,
            })
        });

        Ok(Response::new(Box::pin(out)))
    }

    async fn send_file_data(
        &self,
        request: Request<Streaming<FileData>>,
    ) -> Result<Response<SendFileDataResponse>, Status> {
        let mut stream = request.into_inner();
        let mut current: Option<RaptorBoostTransfer> = None;

        while let Some(file_data) = stream.message().await? {
            if file_data.first {
                if current.is_some() {
                    return Err(Status::invalid_argument(
                        "unexpected 'first' packet before prior transfer completed",
                    ));
                }

                let sha256sum = file_data.sha256sum.as_deref().ok_or_else(|| {
                    Status::invalid_argument("need sha256sum in first data packet")
                })?;
                let force = file_data.force.unwrap_or(false);

                current = Some(self.controller.start_transfer(sha256sum, force).map_err(
                    |e| match e {
                        RaptorBoostError::LockFailure => Status::unavailable("couldn't lock!"),
                        RaptorBoostError::PathSanitization(msg) => Status::invalid_argument(msg),
                        RaptorBoostError::OtherError(msg) => Status::internal(msg),
                        RaptorBoostError::TransferAlreadyComplete => {
                            Status::already_exists("already exists")
                        }
                        _ => Status::internal("unexpected error occurred"),
                    },
                )?);
            }

            let transfer = current
                .as_mut()
                .ok_or_else(|| Status::invalid_argument("first packet not marked as first"))?;

            transfer.write_all(&file_data.data)?;

            if file_data.last {
                current
                    .take()
                    .unwrap()
                    .complete()
                    .map_err(|e| Status::internal(format!("complete failed: {}", e)))?;
            }
        }

        Ok(Response::new(SendFileDataResponse {
            status: SendFileDataStatus::SendfiledatastatusComplete.into(),
        }))
    }

    async fn assign_names(
        &self,
        request: Request<Streaming<AssignNamesRequest>>,
    ) -> Result<Response<AssignNamesResponse>, Status> {
        let now = Local::now();
        let mut stream = request.into_inner();

        let mut header_name: Option<String> = None;
        let mut header_force: bool = false;
        let mut all_sha256_to_filenames: Vec<Sha256Filenames> = Vec::new();
        let mut first = true;

        while let Some(msg) = stream.message().await? {
            if first {
                header_name = msg.name;
                header_force = msg.force.unwrap_or(false);
                first = false;
            }
            all_sha256_to_filenames.extend(msg.sha256_to_filenames);
        }

        let transfer_dir = scoped_join(
            self.controller.get_transfers_dir(),
            match header_name {
                None => format!("{}", now.format("%Y-%m-%d_%H:%M:%S")),
                Some(ref name) => name.clone(),
            },
        )?;

        if header_force {
            let _ = remove_dir_all(&transfer_dir);
        }

        if let Err(e) = create_dir(&transfer_dir) {
            return Err(Status::invalid_argument(format!(
                "couldn't create transfer directory: {}",
                e
            )));
        }

        let complete_dir = self.controller.get_complete_dir();

        for sha256tonames in all_sha256_to_filenames {
            for name in sha256tonames.names {
                let mut path = Path::new(&name);

                if path.has_root() {
                    path = path.strip_prefix("/").unwrap();
                }

                while path.starts_with("..") {
                    path = path.strip_prefix("..").unwrap();
                }

                let dir = path.parent().unwrap();
                let file = path.file_name().unwrap();

                let _ =
                    create_dir_all(transfer_dir.join(scoped_resolve(&transfer_dir, dir).unwrap()));

                let safe_target_sha256sum = complete_dir
                    .join(scoped_resolve(complete_dir, &sha256tonames.sha256sum).unwrap());

                let safe_target_link_dir =
                    transfer_dir.join(scoped_resolve(&transfer_dir, dir).unwrap());
                let safe_target_link =
                    safe_target_link_dir.join(scoped_resolve(&safe_target_link_dir, file).unwrap());

                symlink(safe_target_sha256sum, safe_target_link).unwrap();
            }
        }

        Ok(Response::new(AssignNamesResponse { statuses: vec![] }))
    }
}
