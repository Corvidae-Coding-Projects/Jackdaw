use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use browser_protocol::{
    CommandResult, ErrorCode, ProfileId, ProtocolError, ScreenshotCommand, ScreenshotResult, TabId,
};
use sha2::{Digest, Sha256};

use crate::{
    errors::{internal_error, invalid_request, protocol_error},
    service::BrowserService,
};

impl BrowserService {
    pub(crate) fn handle_screenshot(
        &mut self,
        profile_id: &ProfileId,
        tab_id: TabId,
        command: ScreenshotCommand,
    ) -> Result<CommandResult, ProtocolError> {
        if command.output_path.is_empty() {
            return Err(invalid_request("screenshot output path must not be empty"));
        }

        let owner = self
            .workers
            .envelope(profile_id, &tab_id)
            .ok_or_else(|| internal_error(format!("tab {tab_id} has no document owner")))?;
        let tab = self.tabs.get_mut(&tab_id).ok_or_else(|| {
            protocol_error(
                ErrorCode::TabNotFound,
                format!("tab {tab_id} does not exist"),
            )
        })?;
        if &tab.profile_id != profile_id {
            return Err(protocol_error(
                ErrorCode::TabNotFound,
                format!("tab {tab_id} does not belong to profile {profile_id}"),
            ));
        }

        let rendered = tab.render_document_at(800, 600).map_err(internal_error)?;
        if !self.workers.is_current_envelope(&owner) {
            return Err(internal_error(format!(
                "discarded screenshot from obsolete document owner for tab {tab_id}"
            )));
        }
        let width = rendered.width;
        let height = rendered.height;
        let mut rgba = Vec::with_capacity(rendered.rgba_premultiplied.len());
        for pixel in rendered.rgba_premultiplied.chunks_exact(4) {
            rgba.extend_from_slice(&unpremultiply([pixel[0], pixel[1], pixel[2], pixel[3]]));
        }

        let encoded = encode_png(width, height, &rgba).map_err(internal_error)?;
        let sha256 = format!("{:x}", Sha256::digest(&encoded));
        let output_path = PathBuf::from(&command.output_path);
        publish_atomically(&output_path, &encoded).map_err(|error| {
            internal_error(format!(
                "could not publish screenshot {}: {error}",
                output_path.display()
            ))
        })?;

        Ok(CommandResult::Screenshot(ScreenshotResult {
            path: command.output_path,
            width,
            height,
            bytes: u64::try_from(encoded.len()).unwrap_or(u64::MAX),
            format: "png".to_owned(),
            sha256,
        }))
    }
}

fn unpremultiply([red, green, blue, alpha]: [u8; 4]) -> [u8; 4] {
    if alpha == 0 || alpha == u8::MAX {
        return [red, green, blue, alpha];
    }
    let restore = |channel: u8| {
        let value = u32::from(channel)
            .saturating_mul(u32::from(u8::MAX))
            .saturating_add(u32::from(alpha) / 2)
            / u32::from(alpha);
        u8::try_from(value.min(u32::from(u8::MAX))).expect("clamped channel fits in u8")
    };
    [restore(red), restore(green), restore(blue), alpha]
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoded = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut encoded, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("could not write PNG header: {error}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| format!("could not write PNG pixels: {error}"))?;
        writer
            .finish()
            .map_err(|error| format!("could not finish PNG: {error}"))?;
    }
    Ok(encoded)
}

fn publish_atomically(path: &Path, contents: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    let parent = parent.unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "output path has no file name")
    })?;

    let mut temporary_path = None;
    let mut temporary_file = None;
    for attempt in 0..32_u8 {
        let candidate = parent.join(format!(
            ".{}.rwb-{}-{attempt}.tmp",
            file_name.to_string_lossy(),
            std::process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary_path = Some(candidate);
                temporary_file = Some(file);
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }

    let temporary_path = temporary_path.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a temporary screenshot path",
        )
    })?;
    let mut temporary_file = temporary_file.expect("temporary path and file are created together");
    let result = (|| {
        temporary_file.write_all(contents)?;
        temporary_file.sync_all()?;
        drop(temporary_file);
        fs::hard_link(&temporary_path, path)?;
        fs::remove_file(&temporary_path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary_path);
    }
    result
}
