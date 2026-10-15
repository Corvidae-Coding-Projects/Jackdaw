use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

use browser_protocol::{
    CommandResult, DownloadCommand, DownloadId, DownloadResult, ProfileId, ProtocolError, TabId,
    TransferStatus, UploadCommand,
};
use sha2::{Digest, Sha256};

use crate::{
    errors::{stale_target, tab_not_found, target_not_interactable, transfer_failed},
    service::BrowserService,
};

pub(crate) struct DownloadState {
    pub(crate) profile_id: ProfileId,
    pub(crate) tab_id: TabId,
    pub(crate) body: Vec<u8>,
    pub(crate) sha256: String,
}

impl BrowserService {
    pub(crate) fn handle_upload(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: UploadCommand,
    ) -> Result<CommandResult, ProtocolError> {
        let UploadCommand { target, paths } = command;
        let tab = self
            .tabs
            .get_mut(&tab_id)
            .ok_or_else(|| tab_not_found(&tab_id))?;
        if &tab.profile_id != profile_id {
            return Err(tab_not_found(&tab_id));
        }
        let engine_target = tab
            .observed_targets
            .get(&target)
            .cloned()
            .ok_or_else(|| stale_target(&target))?;
        let mut selected = Vec::with_capacity(paths.len());
        for supplied_path in paths {
            let path = PathBuf::from(&supplied_path);
            let metadata = fs::metadata(&path).map_err(|error| {
                transfer_failed(format!(
                    "could not inspect upload file {supplied_path:?}: {error}"
                ))
            })?;
            if !metadata.is_file() {
                return Err(transfer_failed(format!(
                    "upload path {supplied_path:?} is not a regular file"
                )));
            }
            let file_name = path
                .file_name()
                .and_then(|name| name.to_str())
                .filter(|name| !name.is_empty())
                .ok_or_else(|| transfer_failed("upload path has no UTF-8 file name"))?
                .to_owned();
            let media_type = upload_media_type(&path).to_owned();
            let bytes = fs::read(&path).map_err(|error| {
                transfer_failed(format!(
                    "could not read upload file {supplied_path:?}: {error}"
                ))
            })?;
            selected.push((file_name, media_type, bytes));
        }
        tab.document_worker
            .select_files(&engine_target, selected)
            .map_err(target_not_interactable)?;
        Ok(CommandResult::Empty)
    }

    pub(crate) fn handle_download(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: DownloadCommand,
    ) -> Result<CommandResult, ProtocolError> {
        let result = match command {
            DownloadCommand::Start { url } => self.start_download(profile_id, &tab_id, &url)?,
            DownloadCommand::Wait {
                download_id,
                output_path,
            } => self.finish_download(profile_id, &tab_id, &download_id, &output_path)?,
            DownloadCommand::Cancel { download_id } => {
                let download = self
                    .downloads
                    .get(&download_id)
                    .filter(|download| {
                        &download.profile_id == profile_id && download.tab_id == tab_id
                    })
                    .ok_or_else(|| {
                        transfer_failed(format!("download {download_id} does not exist"))
                    })?;
                let bytes = u64::try_from(download.body.len()).unwrap_or(u64::MAX);
                self.downloads.remove(&download_id);
                DownloadResult {
                    download_id,
                    status: TransferStatus::Cancelled,
                    bytes,
                    sha256: None,
                    path: None,
                }
            }
            DownloadCommand::List => {
                let downloads = self
                    .downloads
                    .iter()
                    .filter(|(_, download)| {
                        &download.profile_id == profile_id && download.tab_id == tab_id
                    })
                    .map(|(download_id, download)| DownloadResult {
                        download_id: download_id.clone(),
                        status: TransferStatus::Completed,
                        bytes: u64::try_from(download.body.len()).unwrap_or(u64::MAX),
                        sha256: Some(download.sha256.clone()),
                        path: None,
                    })
                    .collect();
                return Ok(CommandResult::Downloads(downloads));
            }
        };
        Ok(CommandResult::Download(result))
    }

    fn start_download(
        &mut self,
        profile_id: &ProfileId,
        tab_id: &TabId,
        url: &str,
    ) -> Result<DownloadResult, ProtocolError> {
        let tab = self.tabs.get(tab_id).ok_or_else(|| tab_not_found(tab_id))?;
        if &tab.profile_id != profile_id {
            return Err(tab_not_found(tab_id));
        }
        let response = self
            .broker
            .dispatch_download(profile_id, url)
            .map_err(crate::errors::network_error)?;
        if !(200..300).contains(&response.status) {
            return Err(transfer_failed(format!(
                "download returned HTTP status {}",
                response.status
            )));
        }
        let download_id = DownloadId::parse(format!("d_{}", self.next_download))
            .expect("service-generated download ID is valid");
        self.next_download = self
            .next_download
            .checked_add(1)
            .ok_or_else(|| crate::errors::internal_error("download identifier space exhausted"))?;
        let sha256 = format!("{:x}", Sha256::digest(&response.body));
        let result = DownloadResult {
            download_id: download_id.clone(),
            status: TransferStatus::Completed,
            bytes: u64::try_from(response.body.len()).unwrap_or(u64::MAX),
            sha256: Some(sha256.clone()),
            path: None,
        };
        self.downloads.insert(
            download_id,
            DownloadState {
                profile_id: profile_id.clone(),
                tab_id: tab_id.clone(),
                body: response.body,
                sha256,
            },
        );
        Ok(result)
    }

    fn finish_download(
        &mut self,
        profile_id: &ProfileId,
        tab_id: &TabId,
        download_id: &DownloadId,
        output_path: &str,
    ) -> Result<DownloadResult, ProtocolError> {
        let download = self
            .downloads
            .get(download_id)
            .filter(|download| &download.profile_id == profile_id && &download.tab_id == tab_id)
            .ok_or_else(|| transfer_failed(format!("download {download_id} does not exist")))?;
        let output = PathBuf::from(output_path);
        let parent = output
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        if !parent.is_dir() {
            return Err(transfer_failed(format!(
                "download destination directory {} does not exist",
                parent.display()
            )));
        }
        let file_name = output
            .file_name()
            .ok_or_else(|| transfer_failed("download destination has no file name"))?;
        let partial = parent.join(format!(
            ".{}.{}.partial",
            file_name.to_string_lossy(),
            download_id.as_str()
        ));
        let write_result = (|| -> Result<(), std::io::Error> {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&partial)?;
            file.write_all(&download.body)?;
            file.sync_all()?;
            fs::hard_link(&partial, &output)?;
            fs::remove_file(&partial)?;
            Ok(())
        })();
        if let Err(error) = write_result {
            let _ = fs::remove_file(&partial);
            return Err(transfer_failed(format!(
                "could not publish download to {}: {error}",
                output.display()
            )));
        }
        let result = DownloadResult {
            download_id: download_id.clone(),
            status: TransferStatus::Completed,
            bytes: u64::try_from(download.body.len()).unwrap_or(u64::MAX),
            sha256: Some(download.sha256.clone()),
            path: Some(output_path.to_owned()),
        };
        self.downloads.remove(download_id);
        Ok(result)
    }
}

fn upload_media_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt") => "text/plain",
        Some("json") => "application/json",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        _ => "application/octet-stream",
    }
}
