//! Immutable, host-owned media drafts and native Matrix attachment sending.

use std::{io::Write, path::{Path, PathBuf}, sync::Arc};

use a2app_core::permissions::RoomAccess;
use makepad_widgets::image_cache::image_size_by_data;
use matrix_sdk::{config::RequestConfig, ruma::{OwnedRoomId, serde::Base64, events::room::{MediaSource as MatrixMediaSource, message::{
    AudioMessageEventContent, FileMessageEventContent, ImageMessageEventContent,
    MessageType, RoomMessageEventContent, VideoMessageEventContent,
}}}};
use sha2::{Digest, Sha256};

use super::tools::{MediaDraftRequest, MediaSource, MAX_INLINE_MEDIA_BASE64_BYTES};
use crate::a2app::matrix::policy::{self, MatrixAuthorization};

const MAX_MEDIA_IMAGE_DIMENSION: u64 = 16384;
const MAX_MEDIA_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;

/// The temporary file lives as long as any queued preview/upload owns the
/// draft. Bytes remain immutable and are used directly when uploading.
pub struct PreparedMedia {
    filename: String,
    mime_type: mime::Mime,
    caption: Option<String>,
    bytes: Arc<[u8]>,
    sha256: String,
    path: PathBuf,
    _directory: tempfile::TempDir,
}

impl std::fmt::Debug for PreparedMedia {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PreparedMedia").field("filename", &self.filename)
            .field("mime_type", &self.mime_type).field("size", &self.bytes.len())
            .field("sha256", &self.sha256).finish_non_exhaustive()
    }
}

impl PreparedMedia {
    pub fn from_base64(request: &MediaDraftRequest) -> Result<Self, String> {
        let MediaSource::Base64(encoded) = &request.source else {
            return Err("This media draft needs a downloaded URL response.".into());
        };
        if encoded.is_empty() || encoded.len() > MAX_INLINE_MEDIA_BASE64_BYTES {
            return Err("Inline media exceeds its size limit or is empty.".into());
        }
        let decoded: Base64 = Base64::parse(encoded)
            .map_err(|_| "The media data is not valid base64.".to_string())?;
        Self::from_bytes(request, decoded.into_inner())
    }

    pub fn from_bytes(request: &MediaDraftRequest, bytes: Vec<u8>) -> Result<Self, String> {
        validate_metadata(request)?;
        if bytes.is_empty() || bytes.len() > crate::a2app::network::MAX_MEDIA_RESPONSE_BODY {
            return Err("Media must contain between 1 byte and 16 MiB.".into());
        }
        let mime_type = request.mime_type.parse::<mime::Mime>()
            .map_err(|_| "The media MIME type is invalid.".to_string())?;
        if mime_type.type_() == mime::IMAGE {
            if let Some(actual) = image_mime_from_bytes(&bytes)
                && !image_mimes_match(mime_type.as_ref(), actual)
            { return Err(format!("The image bytes are {actual}, but the draft declares {mime_type}.")); }
            if mime_type.as_ref() == "image/svg+xml" && !makepad_widgets::image_cache::looks_like_svg(&bytes) {
                return Err("The supplied SVG bytes do not contain an SVG document.".into());
            }
        }
        // Unsupported image formats remain usable as native attachments.
        // Only formats decoded by the local preview need raster allocation
        // limits; never reject a vector/document because it has no raster size.
        if mime_type.as_ref() != "image/svg+xml" && crate::image_utils::is_displayable_image(mime_type.as_ref()) {
            if image_mime_from_bytes(&bytes).is_none() {
                return Err("The supplied image bytes are invalid. Use the original Matrix event to decrypt an encrypted attachment.".into());
            }
            let (width, height) = image_size_by_data(&bytes, Path::new(""))
                .map_err(|_| "The supplied image bytes are invalid or unsupported. An encrypted Matrix image needs its original media event and decryption key.")?;
            let (width, height) = (width as u64, height as u64);
            if width > MAX_MEDIA_IMAGE_DIMENSION || height > MAX_MEDIA_IMAGE_DIMENSION
                || width.checked_mul(height).is_none_or(|pixels| pixels > MAX_MEDIA_IMAGE_PIXELS)
            {
                return Err("The image exceeds the media draft limit of 16384 pixels per dimension and 32 million pixels.".into());
            }
        }
        let directory = tempfile::Builder::new().prefix("robrix-agent-media-").tempdir()
            .map_err(|error| format!("Could not create the media draft: {error}"))?;
        let path = directory.path().join(&request.filename);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)] {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path).and_then(|mut file| file.write_all(&bytes))
            .map_err(|error| format!("Could not write the media draft: {error}"))?;
        Ok(Self {
            filename: request.filename.clone(), mime_type,
            caption: request.caption.clone(), sha256: format!("{:x}", Sha256::digest(&bytes)),
            bytes: bytes.into(), path, _directory: directory,
        })
    }

    pub fn path(&self) -> &Path { &self.path }
    pub fn bytes(&self) -> &[u8] { &self.bytes }
    pub fn size(&self) -> usize { self.bytes.len() }
    pub fn filename(&self) -> &str { &self.filename }
    pub fn mime_type(&self) -> &str { self.mime_type.as_ref() }
    pub fn caption(&self) -> Option<&str> { self.caption.as_deref() }

    /// Exact metadata binds permission/action review to the immutable bytes,
    /// without placing a large base64 body into prompts or activity records.
    pub fn metadata(&self) -> serde_json::Value {
        serde_json::json!({"filename":self.filename,"mime_type":self.mime_type.as_ref(),
            "caption":self.caption,"size":self.bytes.len(),"sha256":self.sha256})
    }

    pub fn post_payload(&self, room_id: &OwnedRoomId) -> serde_json::Value {
        serde_json::json!({"room_id":room_id,"media":self.metadata()})
    }

    /// Upload and message posting are separate boundaries: every captured
    /// capability is checked again after each await and before the final post.
    pub async fn post(&self, target: &OwnedRoomId, authorizations: &[MatrixAuthorization]) -> Result<String, String> {
        let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
        self.post_with_client(target, authorizations, client).await
    }

    async fn post_with_client(&self, target: &OwnedRoomId, authorizations: &[MatrixAuthorization], client: matrix_sdk::Client) -> Result<String, String> {
        let primary = authorizations.first().ok_or("Missing media-send authorization.")?.clone();
        validate_post_authorizations(target, authorizations, &self.post_payload(target))?;
        policy::with_authorization(primary.clone(), async {
            let check_permissions = || -> Result<(), String> {
                for authorization in authorizations { authorization.check_current_permission()?; }
                policy::ensure_room_access(target.as_str(), RoomAccess::Write)
            };
            check_permissions()?;
            let room = client.get_room(target).ok_or("Room not found.")?;
            if room.state() != matrix_sdk::RoomState::Joined { return Err("The target room is not joined.".into()); }
            let homeserver = client.homeserver();
            let payload = self.post_payload(target);

            // Approve the immutable media and destination before uploading
            // bytes. Keep once-only authority live until the final send.
            use a2app_core::information_flow::{self as flow, Recipient, SensitiveAction};
            let context = primary.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
            let epoch = primary.flow_epoch.ok_or("Missing information-flow activation.")?;
            let recipient = Recipient::MatrixRoom { account: context.account().into(), room: target.to_string() };
            let action = SensitiveAction { kind: "matrix.media.send".into(), target: target.to_string() };
            review_media_post(&primary, target, &payload).await?;
            let check = || -> Result<(), String> {
                check_permissions()?;
                flow::check_effect_for_activation(context, epoch, Some(&recipient), Some(&action), &payload)
            };
            check()?;

            // Encryption state and upload-limit queries disclose metadata to
            // this server, so review that recipient before either SDK call.
            policy::ensure_server_output_with_payload(homeserver.as_str(), &payload).await?;
            check()?;
            let encrypted = await_checked(&check, policy::audit_server_operation(homeserver.as_str(), room.latest_encryption_state())).await
                .map_err(|error| format!("Could not check room encryption: {error}"))?.is_encrypted();
            check()?;
            policy::ensure_server_output_with_payload(homeserver.as_str(), &payload).await?;
            check()?;
            let max_size = await_checked(&check, policy::audit_server_operation(homeserver.as_str(), client.load_or_fetch_max_upload_size())).await
                .map_err(|error| format!("Could not check the homeserver upload limit: {error}"))?;
            if self.bytes.len() as u64 > u64::from(max_size) { return Err("The media exceeds the homeserver upload limit.".into()); }
            check()?;

            policy::ensure_server_output_with_payload(homeserver.as_str(), &payload).await?;
            check()?;
            let request_config = RequestConfig::new().retry_limit(0);
            // Encryption may have been enabled while metadata or approval
            // was pending. Never ignore a newer encrypted SDK room state.
            let encrypted = encrypted || room.encryption_state().is_encrypted();
            let source = self.upload_source(client.clone(), encrypted, request_config, &check).await?;
            check()?;
            if matches!(&source, MatrixMediaSource::Plain(_)) && room.encryption_state().is_encrypted() {
                return Err("The room enabled encryption during the upload. Post this draft again to upload encrypted media.".into());
            }
            let content = self.event_content(source);
            policy::commit_sensitive_target(target.as_str(), &payload).await?;
            check_permissions()?;
            let sent = await_checked(&check_permissions, policy::audit_room_operation(target.as_str(),
                room.send(content).with_request_config(RequestConfig::new().retry_limit(0)))).await
                .map_err(|error| format!("Could not post media to the room: {error}"))?;
            Ok(serde_json::json!({"room_id":target,"event_id":sent.response.event_id,"media":self.metadata()}).to_string())
        }).await
    }

    async fn upload_source(&self, client: matrix_sdk::Client, encrypted: bool,
        request_config: RequestConfig, check: impl Fn() -> Result<(), String> + Send + Sync) -> Result<MatrixMediaSource, String> {
        if encrypted {
            let mut reader = std::io::Cursor::new(self.bytes.to_vec());
            let file = await_checked(&check, policy::audit_server_operation(client.homeserver().as_str(),
                client.upload_encrypted_file(&mut reader).with_request_config(request_config))).await
                .map_err(|error| format!("Could not upload encrypted media: {error}"))?;
            Ok(MatrixMediaSource::Encrypted(Box::new(file)))
        } else {
            let uploaded = await_checked(&check, policy::audit_server_operation(client.homeserver().as_str(),
                client.media().upload(&self.mime_type, self.bytes.to_vec(), Some(request_config)))).await
                .map_err(|error| format!("Could not upload media: {error}"))?;
            Ok(MatrixMediaSource::Plain(uploaded.content_uri))
        }
    }

    fn event_content(&self, source: MatrixMediaSource) -> RoomMessageEventContent {
        let body = self.caption.clone().filter(|caption| !caption.is_empty()).unwrap_or_else(|| self.filename.clone());
        let size = matrix_sdk::ruma::UInt::try_from(self.bytes.len()).ok();
        let mimetype = Some(self.mime_type.to_string());
        let msgtype = match self.mime_type.type_() {
            mime::IMAGE => {
                let mut info = matrix_sdk::ruma::events::room::ImageInfo::new();
                info.mimetype = mimetype; info.size = size;
                if let Ok((width, height)) = image_size_by_data(self.bytes(), self.path()) {
                    info.width = matrix_sdk::ruma::UInt::try_from(width).ok();
                    info.height = matrix_sdk::ruma::UInt::try_from(height).ok();
                }
                let mut content = ImageMessageEventContent::new(body, source).info(Some(Box::new(info)));
                content.filename = Some(self.filename.clone());
                MessageType::Image(content)
            }
            mime::VIDEO => {
                let mut info = matrix_sdk::ruma::events::room::message::VideoInfo::new();
                info.mimetype = mimetype; info.size = size;
                let mut content = VideoMessageEventContent::new(body, source).info(Some(Box::new(info)));
                content.filename = Some(self.filename.clone());
                MessageType::Video(content)
            }
            mime::AUDIO => {
                let mut info = matrix_sdk::ruma::events::room::message::AudioInfo::new();
                info.mimetype = mimetype; info.size = size;
                let mut content = AudioMessageEventContent::new(body, source).info(Some(Box::new(info)));
                content.filename = Some(self.filename.clone());
                MessageType::Audio(content)
            }
            _ => {
                let mut info = matrix_sdk::ruma::events::room::message::FileInfo::new();
                info.mimetype = mimetype; info.size = size;
                let mut content = FileMessageEventContent::new(body, source).info(Some(Box::new(info)));
                content.filename = Some(self.filename.clone());
                MessageType::File(content)
            }
        };
        RoomMessageEventContent::new(msgtype)
    }
}

fn validate_post_authorizations(target: &OwnedRoomId, authorizations: &[MatrixAuthorization], payload: &serde_json::Value) -> Result<(), String> {
    let primary = authorizations.first().ok_or("Missing media-send authorization.")?;
    let origin = primary.origin_room.as_deref().ok_or("Missing media origin room.")?;
    let context = primary.flow_context.as_ref().ok_or("Missing media activation.")?;
    if primary.capability != "matrix.media.send" || primary.target_room.as_deref() != Some(target.as_str())
        || context.room() != Some(origin) || primary.flow_epoch.is_none_or(|epoch| epoch == 0)
        || primary.flow_payload.as_ref() != Some(payload)
        || !authorizations.iter().any(|auth| auth.capability == "matrix.media.upload")
        || (origin != target.as_str() && !authorizations.iter().any(|auth| auth.capability == "matrix.rooms.message.send"))
        || authorizations.iter().any(|auth| {
            !matches!(auth.capability.as_str(), "matrix.media.send" | "matrix.media.upload" | "matrix.rooms.message.send")
                || auth.subject != primary.subject || auth.subject.is_empty()
                || auth.flow_context != primary.flow_context || auth.flow_epoch != primary.flow_epoch
                || auth.origin_room != primary.origin_room || auth.flow_payload.as_ref() != Some(payload)
                || auth.target_room.as_deref() != Some(if auth.capability == "matrix.media.upload" { origin } else { target.as_str() })
        })
    { return Err("Media permissions must cover this attachment, destination, upload, and activation.".into()); }
    Ok(())
}

/// Drop an in-flight HTTP/SDK future promptly when its captured authority is
/// revoked, instead of waiting for the request timeout to return to the UI.
async fn await_checked<T: Send, E: std::fmt::Display + Send>(check: impl Fn() -> Result<(), String> + Send,
    operation: impl std::future::Future<Output = Result<T, E>> + Send) -> Result<T, String> {
    check()?;
    tokio::pin!(operation);
    loop {
        tokio::select! {
            result = &mut operation => { check()?; return result.map_err(|error| error.to_string()); }
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => check()?,
        }
    }
}

fn image_mimes_match(claimed: &str, actual: &str) -> bool {
    claimed == actual || matches!((claimed, actual),
        ("image/jpg" | "image/pjpeg", "image/jpeg")
        | ("image/x-png" | "image/apng", "image/png")
        | ("image/x-ms-bmp" | "image/x-bmp", "image/bmp")
        | ("image/x-qoi", "image/qoi")
        | ("image/vnd.microsoft.icon", "image/x-icon")
        | ("image/heic" | "image/heif", "image/heic" | "image/heif")
        | ("image/heic-sequence" | "image/heif-sequence", "image/heic-sequence" | "image/heif-sequence"))
}

fn iso_bmff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.get(4..8) != Some(b"ftyp") { return None; }
    let size = u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?);
    let (header, size) = if size == 1 {
        (16, usize::try_from(u64::from_be_bytes(bytes.get(8..16)?.try_into().ok()?)).ok()?)
    } else { (8, usize::try_from(size).ok()?) };
    // Read only the declared file-type box, never a codec-looking string in
    // later media payloads. Bound brand scans even for malformed attachments.
    if size < header + 8 || size > 4096 || size > bytes.len() || (size - header - 8) % 4 != 0 { return None; }
    let major = bytes.get(header..header + 4)?;
    let compatible = bytes.get(header + 8..size)?;
    let has_brand = |brand: &[u8]| major == brand || compatible.chunks_exact(4).any(|candidate| candidate == brand);
    if has_brand(b"avif") || has_brand(b"avis") { Some("image/avif") }
    else if [b"heic", b"heix", b"heim", b"heis"].iter().any(|brand| has_brand(*brand)) { Some("image/heic") }
    else if [b"hevc", b"hevx", b"hevm", b"hevs"].iter().any(|brand| has_brand(*brand)) { Some("image/heic-sequence") }
    else if has_brand(b"mif1") { Some("image/heif") }
    else if has_brand(b"msf1") { Some("image/heif-sequence") }
    else { None }
}

fn image_mime_from_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") { Some("image/png") }
    else if bytes.starts_with(b"\xff\xd8\xff") { Some("image/jpeg") }
    else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") { Some("image/gif") }
    else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") { Some("image/webp") }
    else if bytes.starts_with(b"BM") { Some("image/bmp") }
    else if bytes.starts_with(b"qoif") { Some("image/qoi") }
    else if bytes.starts_with(b"\0\0\x01\0") { Some("image/x-icon") }
    else if bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") { Some("image/tiff") }
    else if bytes.get(4..8) == Some(b"ftyp") { iso_bmff_image_mime(bytes) }
    else if makepad_widgets::image_cache::looks_like_svg(bytes) { Some("image/svg+xml") }
    else { None }
}

/// Review posting without consuming its once-only authority during upload.
async fn review_media_post(authorization: &MatrixAuthorization, target: &OwnedRoomId, payload: &serde_json::Value) -> Result<(), String> {
    use a2app_core::information_flow::{self as flow, Recipient, SensitiveAction};
    let context = authorization.flow_context.as_ref().ok_or("Missing host information-flow context.")?;
    let epoch = authorization.flow_epoch.ok_or("Missing information-flow activation.")?;
    let recipient = Recipient::MatrixRoom { account: context.account().into(), room: target.to_string() };
    let action = SensitiveAction { kind: "matrix.media.send".into(), target: target.to_string() };
    let review = flow::prepare_effect_for_activation(context, epoch, Some(&recipient), Some(&action), payload)?;
    crate::a2app::effect_review::request(review, true).await?;
    flow::check_effect_for_activation(context, epoch, Some(&recipient), Some(&action), payload)
}

fn validate_metadata(request: &MediaDraftRequest) -> Result<(), String> {
    let filename = &request.filename;
    if filename.is_empty() || filename.len() > 255 || filename == "." || filename == ".."
        || filename.chars().any(|ch| ch.is_control() || ch == '/' || ch == '\\')
    { return Err("The media filename must be a simple filename with no path.".into()); }
    let mime_type = request.mime_type.parse::<mime::Mime>().map_err(|_| "The media MIME type is invalid.".to_string())?;
    if request.mime_type.len() > 255 || mime_type.type_() == mime::STAR || mime_type.subtype() == mime::STAR
        || mime_type.params().next().is_some()
    {
        return Err("The media MIME type must be concrete.".into());
    }
    if request.caption.as_ref().is_some_and(|caption| caption.len() > 16 * 1024) {
        return Err("The media caption exceeds its size limit.".into());
    }
    Ok(())
}

/// A bare URI must match an actual cached media event in the authorized
/// source room. Otherwise require an event ID that establishes its owner.
pub async fn download_mxc(uri: &str, authorization: MatrixAuthorization) -> Result<Vec<u8>, String> {
    policy::with_authorization(authorization.clone(), async {
        authorization.check_current_permission()?;
        let source_room = authorization.target_room.as_deref().ok_or("Missing media source room.")?;
        let source_room = OwnedRoomId::try_from(source_room).map_err(|_| "Invalid media source room.")?;
        validate_download_authorization(&source_room, &authorization)?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
        let room = client.get_room(&source_room).ok_or("The media source room was not found.")?;
        if room.state() != matrix_sdk::RoomState::Joined { return Err("The media source room is not joined.".into()); }
        let (cache, _guard) = client.event_cache().room(&source_room).await
            .map_err(|_| "The media source room cache is unavailable.")?;
        let events = cache.events().await.map_err(|_| "Could not read the media source room cache.")?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let descriptor = resolve_cached_media_uri(uri, &source_room, &events)
            .ok_or("Use draft_media with event_id and source_room to identify the authorized attachment.")?;
        download_descriptor(descriptor, authorization, client).await
    }).await
}

/// Download plaintext MXC media through the logged-in account's own
/// authenticated homeserver endpoint. A dedicated client prevents redirects
/// from forwarding the access token and bounds allocation while streaming.
async fn raw_download_mxc(uri: &str, authorization: MatrixAuthorization, client: matrix_sdk::Client) -> Result<Vec<u8>, String> {
    policy::with_authorization(authorization.clone(), async {
        authorization.check_current_permission()?;
        let mxc: &matrix_sdk::ruma::MxcUri = uri.into();
        let (server, media_id) = mxc.parts().map_err(|_| "The Matrix media URI is invalid.".to_string())?;
        let mut destination = client.homeserver();
        destination.set_query(None);
        destination.set_fragment(None);
        destination.path_segments_mut().map_err(|_| "The homeserver URL cannot contain media paths.")?
            .pop_if_empty().extend(["_matrix", "client", "v1", "media", "download", server.as_str(), media_id]);
        let token = client.access_token().ok_or("The Matrix account has no access token.")?;
        let payload = serde_json::json!({"operation":"matrix.media.download","mxc_uri":uri});
        policy::ensure_server_output_with_payload(client.homeserver().as_str(), &payload).await?;
        authorization.check_current_permission()?;
        let http = matrix_sdk::reqwest::Client::builder()
            .no_proxy().redirect(matrix_sdk::reqwest::redirect::Policy::none())
            .retry(matrix_sdk::reqwest::retry::never()).referer(false)
            .no_gzip().no_brotli().no_deflate().no_zstd()
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build().map_err(|_| "Could not create the Matrix media download client.")?;
        let check = || authorization.check_current_permission();
        let mut response = await_checked(&check, policy::audit_server_operation(client.homeserver().as_str(),
            http.get(destination).bearer_auth(token).header(matrix_sdk::reqwest::header::ACCEPT_ENCODING, "identity").send())).await
            .map_err(|_| "The Matrix media download failed.".to_string())?;
        authorization.check_current_permission()?;
        if response.status().is_redirection() { return Err("The homeserver redirected the media download; redirects are not followed.".into()); }
        if !response.status().is_success() { return Err(format!("The homeserver refused the media download (HTTP {}).", response.status().as_u16())); }
        let limit = crate::a2app::network::MAX_MEDIA_RESPONSE_BODY;
        if response.content_length().is_some_and(|length| length > limit as u64) {
            return Err("The Matrix media exceeds the 16 MiB download limit.".into());
        }
        let mut bytes = Vec::new();
        loop {
            authorization.check_current_permission()?;
            let chunk = await_checked(&check, response.chunk()).await.map_err(|error| format!("Reading Matrix media failed: {error}"))?;
            authorization.check_current_permission()?;
            let Some(chunk) = chunk else { break };
            if chunk.len() > limit - bytes.len() { return Err("The Matrix media exceeds the 16 MiB download limit.".into()); }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }).await
}

/// Resolve the actual media descriptor from a decrypted event in its
/// authorized source room, then verify and decrypt attachment ciphertext.
pub async fn download_event_media(
    source_room: &OwnedRoomId,
    event_id: &str,
    authorization: MatrixAuthorization,
) -> Result<Vec<u8>, String> {
    Ok(download_event_attachment(source_room, event_id, authorization).await?.bytes)
}

/// Content and metadata come from the actual decrypted Matrix event. This
/// avoids requiring the caller to guess a MIME type before inspecting it.
pub struct DownloadedMedia {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub mime_type: String,
    pub caption: Option<String>,
}

impl DownloadedMedia {
    pub fn prepare(self) -> Result<PreparedMedia, String> {
        PreparedMedia::from_bytes(&MediaDraftRequest {
            source: MediaSource::Base64(String::new()), filename: self.filename,
            mime_type: self.mime_type, caption: self.caption,
        }, self.bytes)
    }

    pub fn wire_result(self, source_room: &OwnedRoomId, event_id: &str) -> Result<String, String> {
        let media = self.prepare()?;
        let data: Base64 = Base64::new(media.bytes().to_vec());
        Ok(serde_json::json!({"room_id":source_room,"event_id":event_id,
            "data_base64":data.encode(),
            "filename":media.filename(),"mime_type":media.mime_type(),"size":media.size(),
            "caption":media.caption(),"sha256":media.sha256}).to_string())
    }
}

pub async fn download_event_attachment(
    source_room: &OwnedRoomId,
    event_id: &str,
    authorization: MatrixAuthorization,
) -> Result<DownloadedMedia, String> {
    let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
    download_event_attachment_with_client(source_room, event_id, authorization, client).await
}

async fn download_event_attachment_with_client(
    source_room: &OwnedRoomId,
    event_id: &str,
    authorization: MatrixAuthorization,
    client: matrix_sdk::Client,
) -> Result<DownloadedMedia, String> {
    use matrix_sdk::ruma::OwnedEventId;
    policy::with_authorization(authorization.clone(), async {
        validate_download_authorization(source_room, &authorization)?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let event_id = OwnedEventId::try_from(event_id).map_err(|_| "The media event ID is invalid.")?;
        let room = client.get_room(source_room).ok_or("The media source room was not found.")?;
        if room.state() != matrix_sdk::RoomState::Joined { return Err("The media source room is not joined.".into()); }
        let payload = serde_json::json!({"operation":"matrix.media.download","room_id":source_room,"event_id":event_id});
        policy::ensure_server_output_with_payload(client.homeserver().as_str(), &payload).await?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let check = || { validate_download_authorization(source_room, &authorization)?; policy::ensure_room_access(source_room.as_str(), RoomAccess::Read) };
        let event = await_checked(&check, policy::audit_server_operation(client.homeserver().as_str(),
            room.event(&event_id, Some(RequestConfig::new().retry_limit(0))))).await
            .map_err(|error| format!("Could not fetch the media event: {error}"))?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let (actual_event_id, source, size) = media_source_from_event(&event, source_room)
            .ok_or("The selected event is not an available media message in the source room.")?;
        if actual_event_id != event_id { return Err("The homeserver returned a different media event.".into()); }
        let (filename, mime_type, caption) = media_metadata_from_event(&event)
            .ok_or("The selected event has no valid attachment metadata.")?;
        let bytes = download_descriptor((source, size), authorization, client).await?;
        let mime_type = if mime_type.is_empty() || mime_type == "application/octet-stream" {
            inferred_media_mime(&filename, &bytes)
        } else { mime_type };
        Ok(DownloadedMedia { bytes, filename, mime_type, caption })
    }).await
}

fn inferred_media_mime(filename: &str, bytes: &[u8]) -> String {
    if let Some(mime_type) = image_mime_from_bytes(bytes) { return mime_type.into(); }
    if bytes.starts_with(b"%PDF-") { return "application/pdf".into(); }
    if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WAVE") { return "audio/wav".into(); }
    if bytes.starts_with(b"fLaC") { return "audio/flac".into(); }
    let guessed = mime_guess::from_path(filename).first_or_octet_stream();
    if guessed != mime::APPLICATION_OCTET_STREAM { return guessed.to_string(); }
    if bytes.starts_with(b"OggS") { return "audio/ogg".into(); }
    if bytes.starts_with(b"ID3") { return "audio/mpeg".into(); }
    if bytes.get(4..8) == Some(b"ftyp") { return "video/mp4".into(); }
    "application/octet-stream".into()
}

fn validate_download_authorization(source_room: &OwnedRoomId, authorization: &MatrixAuthorization) -> Result<(), String> {
    if authorization.capability != "matrix.media.download" || authorization.target_room.as_deref() != Some(source_room.as_str()) {
        return Err("The media download permission does not match its source room.".into());
    }
    authorization.check_current_permission()
}

fn media_metadata_from_event(event: &matrix_sdk::deserialized_responses::TimelineEvent) -> Option<(String, String, Option<String>)> {
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(message))) = event.raw().deserialize().ok()?
    else { return None; };
    let (body, filename, mime_type) = match message.content.msgtype {
        MessageType::Image(content) => (content.body, content.filename, content.info.and_then(|info| info.mimetype)),
        MessageType::Video(content) => (content.body, content.filename, content.info.and_then(|info| info.mimetype)),
        MessageType::Audio(content) => (content.body, content.filename, content.info.and_then(|info| info.mimetype)),
        MessageType::File(content) => (content.body, content.filename, content.info.and_then(|info| info.mimetype)),
        _ => return None,
    };
    let mime_type = mime_type.unwrap_or_else(|| "application/octet-stream".into());
    let mime_type = mime_type.split(';').next()?.trim().to_ascii_lowercase();
    // Original events may use a caption as `body` and omit `filename`.
    // Derive a safe display filename for host-owned bytes rather than treating
    // that caption as a filesystem path. Caller-supplied draft names remain
    // subject to strict metadata validation.
    let mut filename = sanitize_filename::sanitize(filename.as_deref().filter(|name| !name.is_empty()).unwrap_or(&body));
    if filename.trim().is_empty() || matches!(filename.as_str(), "." | "..") { filename = "attachment".into(); }
    while filename.len() > 200 { filename.pop(); }
    if Path::new(&filename).extension().is_none()
        && let Some(extension) = mime_guess::get_mime_extensions_str(&mime_type).and_then(|extensions| extensions.first())
    { filename.push('.'); filename.push_str(extension); }
    let caption = (body != filename).then_some(body);
    Some((filename, mime_type, caption))
}

fn media_source_from_event(
    event: &matrix_sdk::deserialized_responses::TimelineEvent,
    source_room: &OwnedRoomId,
) -> Option<(matrix_sdk::ruma::OwnedEventId, MatrixMediaSource, Option<matrix_sdk::ruma::UInt>)> {
    use matrix_sdk::ruma::events::{AnySyncMessageLikeEvent, AnySyncTimelineEvent, SyncMessageLikeEvent};
    if event.raw().get_field::<OwnedRoomId>("room_id").ok().flatten()
        .is_some_and(|room_id| room_id != *source_room)
    { return None; }
    let AnySyncTimelineEvent::MessageLike(AnySyncMessageLikeEvent::RoomMessage(SyncMessageLikeEvent::Original(message))) = event.raw().deserialize().ok()?
    else { return None; };
    let (source, size) = match message.content.msgtype {
        MessageType::Image(content) => (content.source, content.info.and_then(|info| info.size)),
        MessageType::Video(content) => (content.source, content.info.and_then(|info| info.size)),
        MessageType::Audio(content) => (content.source, content.info.and_then(|info| info.size)),
        MessageType::File(content) => (content.source, content.info.and_then(|info| info.size)),
        _ => return None,
    };
    Some((message.event_id, source, size))
}

fn resolve_cached_media_uri(
    uri: &str,
    source_room: &OwnedRoomId,
    events: &[matrix_sdk::deserialized_responses::TimelineEvent],
) -> Option<(MatrixMediaSource, Option<matrix_sdk::ruma::UInt>)> {
    events.iter().rev().find_map(|event| {
        let (_, source, size) = media_source_from_event(event, source_room)?;
        let original_uri = match &source {
            MatrixMediaSource::Plain(uri) => uri,
            MatrixMediaSource::Encrypted(file) => &file.url,
        };
        (original_uri.as_str() == uri).then_some((source, size))
    })
}

async fn download_descriptor(
    descriptor: (MatrixMediaSource, Option<matrix_sdk::ruma::UInt>),
    authorization: MatrixAuthorization,
    client: matrix_sdk::Client,
) -> Result<Vec<u8>, String> {
    let (source, size) = descriptor;
    if size.is_some_and(|size| u64::from(size) > crate::a2app::network::MAX_MEDIA_RESPONSE_BODY as u64) {
        return Err("The Matrix media exceeds the 16 MiB download limit.".into());
    }
    let uri = match &source {
        MatrixMediaSource::Plain(uri) => uri,
        MatrixMediaSource::Encrypted(file) => &file.url,
    };
    let bytes = raw_download_mxc(uri.as_str(), authorization.clone(), client).await?;
    authorization.check_current_permission()?;
    match source {
        MatrixMediaSource::Plain(_) => Ok(bytes),
        MatrixMediaSource::Encrypted(file) => decrypt_media(bytes, (*file).into()),
    }
}

fn decrypt_media(bytes: Vec<u8>, encryption: matrix_sdk_base::crypto::MediaEncryptionInfo) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut reader = std::io::Cursor::new(bytes);
    let mut decryptor = matrix_sdk_base::crypto::AttachmentDecryptor::new(&mut reader, encryption)
        .map_err(|_| "The attachment encryption information is invalid.")?;
    let mut decrypted = Vec::new();
    decryptor.read_to_end(&mut decrypted).map_err(|_| "The attachment could not be decrypted or its hash did not match.")?;
    Ok(decrypted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(filename: &str, mime_type: &str, bytes: &[u8]) -> MediaDraftRequest {
        let encoded: Base64 = Base64::new(bytes.to_vec());
        MediaDraftRequest { source: MediaSource::Base64(encoded.encode()),
            filename: filename.into(), mime_type: mime_type.into(), caption: Some("Agent caption".into()) }
    }

    #[test]
    fn declared_image_mime_must_match_bytes_and_vector_files_are_supported() {
        let png = include_bytes!("../../../resources/icon_32.png");
        assert!(PreparedMedia::from_base64(&request("wrong.jpeg", "image/jpeg", png)).unwrap_err().contains("image/png"));
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><rect width="16" height="16"/></svg>"#;
        let media = PreparedMedia::from_base64(&request("drawing.svg", "image/svg+xml", svg)).unwrap();
        assert_eq!(media.bytes(), svg);
        assert!(PreparedMedia::from_base64(&request("wrong.svg", "image/svg+xml", b"encrypted ciphertext")).is_err());
    }

    #[test]
    fn omitted_matrix_mime_is_recovered_from_signature_or_original_filename() {
        for (filename, bytes, expected) in [
            ("unknown", b"%PDF-1.4".as_slice(), "application/pdf"),
            ("recording", b"RIFF\x10\0\0\0WAVE".as_slice(), "audio/wav"),
            ("clip", b"\0\0\0\x18ftypmp42".as_slice(), "video/mp4"),
            ("note.txt", b"Actual text".as_slice(), "text/plain"),
            ("document.pdf", b"pdf bytes".as_slice(), "application/pdf"),
            ("recording.mp3", b"audio bytes".as_slice(), "audio/mpeg"),
            ("unknown.bin", b"\0\x01\xff".as_slice(), "application/octet-stream"),
        ] { assert_eq!(inferred_media_mime(filename, bytes), expected); }
    }

    #[test]
    fn iso_bmff_image_brands_are_bounded_and_distinct_from_video() {
        let box_bytes = |major: &[u8; 4], compatible: &[[u8; 4]]| {
            let mut bytes = ((16 + 4 * compatible.len()) as u32).to_be_bytes().to_vec();
            bytes.extend_from_slice(b"ftyp");
            bytes.extend_from_slice(major);
            bytes.extend_from_slice(&[0; 4]);
            for brand in compatible { bytes.extend_from_slice(brand); }
            bytes
        };
        for (major, compatible, expected) in [
            (*b"heic", vec![*b"mif1"], "image/heic"),
            (*b"mif1", vec![*b"heix"], "image/heic"),
            (*b"mif1", vec![*b"avif"], "image/avif"),
            (*b"msf1", vec![*b"avis"], "image/avif"),
            (*b"hevx", vec![*b"msf1"], "image/heic-sequence"),
            (*b"mif1", vec![], "image/heif"),
            (*b"msf1", vec![], "image/heif-sequence"),
        ] { assert_eq!(inferred_media_mime("caption without extension", &box_bytes(&major, &compatible)), expected); }
        let mut mp4 = box_bytes(b"mp42", &[*b"isom"]);
        mp4.extend_from_slice(b"heicavif");
        assert_eq!(image_mime_from_bytes(&mp4), None, "payload bytes outside ftyp are not brands");
        assert_eq!(inferred_media_mime("clip", &mp4), "video/mp4");
        assert_eq!(inferred_media_mime("clip", include_bytes!("../../../a2app/agent/tests/fixtures/blue.mp4")), "video/mp4");
        let valid = box_bytes(b"heic", &[*b"mif1"]);
        assert_eq!(image_mime_from_bytes(&valid[..valid.len() - 1]), None);
        let mut malformed = valid.clone();
        malformed[..4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert_eq!(image_mime_from_bytes(&malformed), None);
        malformed[..4].copy_from_slice(&17u32.to_be_bytes());
        assert_eq!(image_mime_from_bytes(&malformed), None);
        let mut extended = 1u32.to_be_bytes().to_vec();
        extended.extend_from_slice(b"ftyp");
        extended.extend_from_slice(&((valid.len() + 8) as u64).to_be_bytes());
        extended.extend_from_slice(&valid[8..]);
        assert_eq!(image_mime_from_bytes(&extended), Some("image/heic"));
        assert!(image_mimes_match("image/heif", "image/heic"));
        assert!(image_mimes_match("image/heic", "image/heif"));
        assert!(!image_mimes_match("image/heic", "image/avif"));
    }

    #[test]
    fn original_caption_is_preserved_when_event_filename_is_missing_or_unsafe() {
        for caption in ["boat / river", "notes\nnext line", ""] {
            let value = serde_json::json!({"type":"m.room.message","event_id":"$caption","sender":"@media:test","origin_server_ts":1,
                "content":{"msgtype":"m.image","body":caption,"url":"mxc://media.test/caption","info":{"mimetype":"image/png"}}});
            let event = matrix_sdk::deserialized_responses::TimelineEvent::from_plaintext(
                matrix_sdk::ruma::serde::Raw::from_json_string(value.to_string()).unwrap());
            let (filename, mime_type, actual_caption) = media_metadata_from_event(&event).unwrap();
            assert!(!filename.contains(['/', '\\', '\n']));
            assert!(filename.ends_with(".png"));
            assert_eq!(mime_type, "image/png");
            assert_eq!(actual_caption.as_deref(), Some(caption));
            let prepared = DownloadedMedia { bytes: include_bytes!("../../../resources/icon_32.png").to_vec(), filename, mime_type, caption: actual_caption }.prepare().unwrap();
            assert_eq!(prepared.caption(), Some(caption));
        }
    }

    /// An actual SDK client talks to this isolated homeserver. Its event,
    /// authenticated media-download, config, upload, and native-send routes
    /// retain the exact requests for assertions; no real account is used.
    struct MockHomeserver {
        url: String,
        events: Arc<std::sync::Mutex<std::collections::BTreeMap<String, serde_json::Value>>>,
        media: Arc<std::sync::Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
        requests: Arc<std::sync::Mutex<Vec<(String, Vec<u8>)>>>,
        encrypted: Arc<std::sync::atomic::AtomicBool>,
        stop: Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl MockHomeserver {
        fn new() -> Self {
            use std::{io::{Read, Write}, net::TcpListener, sync::{Mutex, atomic::{AtomicBool, Ordering}}};
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let events = Arc::new(Mutex::new(std::collections::BTreeMap::<String, serde_json::Value>::new()));
            let media = Arc::new(Mutex::new(std::collections::BTreeMap::<String, Vec<u8>>::new()));
            let requests = Arc::new(Mutex::new(Vec::new()));
            let encrypted = Arc::new(AtomicBool::new(false));
            let stop = Arc::new(AtomicBool::new(false));
            let (event_map, media_map, captured, stopping, room_encrypted) = (events.clone(), media.clone(), requests.clone(), stop.clone(), encrypted.clone());
            let thread = std::thread::spawn(move || {
                while !stopping.load(Ordering::Acquire) {
                    let (mut stream, _) = match listener.accept() {
                        Ok(stream) => stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => { std::thread::sleep(std::time::Duration::from_millis(5)); continue; }
                        Err(_) => break,
                    };
                    // macOS inherits O_NONBLOCK from the listener; normalize
                    // the accepted stream before its bounded blocking reads.
                    stream.set_nonblocking(false).unwrap();
                    stream.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
                    let mut input = Vec::new();
                    let header_end = loop {
                        let mut buffer = [0; 4096];
                        let count = match stream.read(&mut buffer) { Ok(count) if count > 0 => count, _ => break None };
                        input.extend_from_slice(&buffer[..count]);
                        if let Some(offset) = input.windows(4).position(|window| window == b"\r\n\r\n") { break Some(offset + 4); }
                    };
                    let Some(header_end) = header_end else { continue };
                    let header = String::from_utf8_lossy(&input[..header_end]).into_owned();
                    let length = header.lines().find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length").then(|| value.trim().parse::<usize>().ok()).flatten()
                    }).unwrap_or(0);
                    while input.len() < header_end + length {
                        let mut buffer = [0; 4096];
                        match stream.read(&mut buffer) { Ok(count) if count > 0 => input.extend_from_slice(&buffer[..count]), _ => break }
                    }
                    let body = input[header_end..].to_vec();
                    captured.lock().unwrap().push((header.clone(), body));
                    let path = header.lines().next().unwrap().split_whitespace().nth(1).unwrap();
                    let (status, response) = if path.contains("/sync") {
                        let mut sync = serde_json::json!({"next_batch":"mock-token","rooms":{"join":{"!media:test":{
                            "state":{"events":[{"type":"m.room.member","state_key":"@media:test","sender":"@media:test","event_id":"$member","origin_server_ts":1,"content":{"membership":"join"}}]},
                            "timeline":{"events":[],"limited":false,"prev_batch":"mock-prev"}
                        }}}});
                        if room_encrypted.load(Ordering::Acquire) {
                            sync["rooms"]["join"]["!media:test"]["state"]["events"].as_array_mut().unwrap().push(serde_json::json!({
                                "type":"m.room.encryption","state_key":"","sender":"@media:test","event_id":"$encryption","origin_server_ts":1,
                                "content":{"algorithm":"m.megolm.v1.aes-sha2"}}));
                        }
                        (200, sync.to_string().into_bytes())
                    } else if path.contains("/event/") {
                        let events = event_map.lock().unwrap();
                        match events.iter().find(|(id, _)| path.contains(id.as_str())) {
                            Some((_, event)) => (200, event.to_string().into_bytes()),
                            None => (404, br#"{"errcode":"M_NOT_FOUND","error":"missing event"}"#.to_vec()),
                        }
                    } else if path.contains("/media/download/") {
                        let media = media_map.lock().unwrap();
                        match media.iter().find(|(id, _)| path.contains(id.as_str())) {
                            Some((_, bytes)) => (200, bytes.clone()),
                            None => (404, br#"{"errcode":"M_NOT_FOUND","error":"missing media"}"#.to_vec()),
                        }
                    } else if path.contains("/config") {
                        (200, br#"{"m.upload.size":16777216}"#.to_vec())
                    } else if path.contains("/media/") && path.contains("/upload") {
                        (200, br#"{"content_uri":"mxc://media.test/uploaded"}"#.to_vec())
                    } else if path.contains("/send/m.room.message/") || path.contains("/send/m.room.encrypted/") {
                        (200, br#"{"event_id":"$posted"}"#.to_vec())
                    } else if path.contains("/state/m.room.encryption") && room_encrypted.load(Ordering::Acquire) {
                        (200, br#"{"algorithm":"m.megolm.v1.aes-sha2"}"#.to_vec())
                    } else if path.contains("/members") {
                        (200, br#"{"chunk":[{"type":"m.room.member","state_key":"@media:test","sender":"@media:test","event_id":"$member","room_id":"!media:test","origin_server_ts":1,"content":{"membership":"join"}}]}"#.to_vec())
                    } else if path.contains("/keys/query") {
                        (200, br#"{"device_keys":{},"failures":{}}"#.to_vec())
                    } else if path.contains("/keys/claim") {
                        (200, br#"{"one_time_keys":{},"failures":{}}"#.to_vec())
                    } else if path.contains("/keys/upload") {
                        (200, br#"{"one_time_key_counts":{"signed_curve25519":50}}"#.to_vec())
                    } else if path.contains("/sendToDevice/") {
                        (200, b"{}".to_vec())
                    } else {
                        (404, br#"{"errcode":"M_NOT_FOUND","error":"missing state"}"#.to_vec())
                    };
                    let _ = write!(stream, "HTTP/1.1 {status} Mock\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", response.len());
                    let _ = stream.write_all(&response);
                }
            });
            Self { url, events, media, requests, encrypted, stop, thread: Some(thread) }
        }

        async fn client(&self) -> matrix_sdk::Client {
            use matrix_sdk::{authentication::matrix::MatrixSession, ruma::{owned_device_id, owned_user_id, api::MatrixVersion}, SessionMeta, SessionTokens};
            let http = matrix_sdk::reqwest::Client::builder().no_proxy().build().unwrap();
            let client = matrix_sdk::Client::builder().homeserver_url(&self.url).http_client(http)
                .server_versions([MatrixVersion::V1_11]).build().await.unwrap();
            client.restore_session(MatrixSession { meta: SessionMeta {
                user_id: owned_user_id!("@media:test"), device_id: owned_device_id!("MOCKMEDIA"),
            }, tokens: SessionTokens { access_token: "mock-media-token".into(), refresh_token: None } }).await.unwrap();
            client.sync_once(matrix_sdk::config::SyncSettings::default().timeout(std::time::Duration::ZERO)).await.unwrap();
            assert!(client.get_room(&OwnedRoomId::try_from("!media:test").unwrap()).is_some());
            client
        }
    }

    impl Drop for MockHomeserver {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Release);
            if let Some(thread) = self.thread.take() { thread.join().unwrap(); }
        }
    }

    struct MockPermissionContext {
        context: a2app_core::information_flow::ContextId,
        store: a2app_core::permissions::PermissionStore,
        previous_account: Option<String>,
    }

    impl MockPermissionContext {
        fn new(homeserver: &str) -> Self {
            use a2app_core::{information_flow as flow, permissions::{GrantDuration, Permission, PermissionStore, RoomScope}};
            let previous_account = crate::a2app::information_flow::TEST_ACCOUNT.with(|account| account.replace(Some("@media:test".into())));
            let context = crate::a2app::information_flow::begin_agent_session("!media:test").unwrap();
            let subject = a2app_core::permissions::agent_subject("!media:test");
            let mut store = PermissionStore::default();
            store.set_matrix_write(true);
            for capability in ["matrix.media.download", "matrix.media.upload", "matrix.media.send"] {
                store.grant_scoped(&subject, Permission::MatrixMedia, Some(capability), RoomScope::room("!media:test"), GrantDuration::Always, None).unwrap();
            }
            policy::publish_permission_policy(&store);
            for source in flow::labels(&context).unwrap() {
                flow::grant_sharing(source, flow::Recipient::network_origin(homeserver).unwrap(),
                    flow::ReaderScope::Context(context.clone()), flow::SharingDuration::RobrixSession).unwrap();
            }
            Self { context, store, previous_account }
        }

        fn auth(&self, capability: &str) -> MatrixAuthorization {
            MatrixAuthorization::new(&a2app_core::permissions::agent_subject("!media:test"), capability, Some("!media:test"), &self.store).with_flow(self.context.clone())
        }
    }

    impl Drop for MockPermissionContext {
        fn drop(&mut self) {
            a2app_core::information_flow::remove_context(&self.context).unwrap();
            policy::publish_permission_policy(&a2app_core::permissions::PermissionStore::default());
            crate::a2app::information_flow::TEST_ACCOUNT.with(|account| { account.replace(self.previous_account.take()); });
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn sdk_event_to_authenticated_download_decrypts_all_native_media_types() {
        use std::io::Read;
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let fixture = MockPermissionContext::new(&server.url);
        let room = OwnedRoomId::try_from("!media:test").unwrap();
        let samples: &[(&str, &str, &str, &[u8])] = &[
            ("m.image", "picture.png", "image/png", include_bytes!("../../../resources/icon_32.png")),
            ("m.audio", "recording.wav", "audio/wav", include_bytes!("../../../a2app/agent/tests/fixtures/tone.wav")),
            ("m.video", "clip.mp4", "video/mp4", include_bytes!("../../../a2app/agent/tests/fixtures/blue.mp4")),
            ("m.file", "report.pdf", "application/pdf", include_bytes!("../../../a2app/agent/tests/fixtures/report.pdf")),
            ("m.image", "snapshot.heic", "image/heic", include_bytes!("../../../a2app/agent/tests/fixtures/vision-colors.heic")),
        ];
        for (index, (msgtype, filename, mime_type, original)) in samples.iter().enumerate() {
            for encrypted in [false, true] {
                let id = format!("fixture{index}{}", u8::from(encrypted));
                let uri = format!("mxc://media.test/{id}");
                let mut content = serde_json::json!({"msgtype":msgtype,"body":"A caption","filename":filename,
                    "info":{"mimetype":mime_type,"size":original.len()}});
                if !encrypted { content["info"].as_object_mut().unwrap().remove("mimetype"); }
                let caption = if *mime_type == "image/heic" {
                    // iPhone images can have only a caption, with no filename
                    // extension or MIME metadata, including encrypted sources.
                    content.as_object_mut().unwrap().remove("filename");
                    content["info"].as_object_mut().unwrap().remove("mimetype");
                    content["body"] = "Photo / river".into();
                    "Photo / river"
                } else { "A caption" };
                let bytes = if encrypted {
                    let mut reader = std::io::Cursor::new(*original);
                    let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(&mut reader);
                    let mut bytes = Vec::new();
                    encryptor.read_to_end(&mut bytes).unwrap();
                    let mut file = serde_json::to_value(encryptor.finish()).unwrap();
                    file["url"] = uri.clone().into();
                    content["file"] = file;
                    bytes
                } else { content["url"] = uri.clone().into(); original.to_vec() };
                server.media.lock().unwrap().insert(id.clone(), bytes);
                server.events.lock().unwrap().insert(id.clone(), serde_json::json!({"type":"m.room.message","room_id":room,
                    "event_id":format!("${id}"),"sender":"@media:test","origin_server_ts":1,"content":content}));
                let media = download_event_attachment_with_client(&room, &format!("${id}"), fixture.auth("matrix.media.download"), client.clone()).await.unwrap();
                assert_eq!(media.bytes, *original);
                if *mime_type == "image/heic" { assert!(!media.filename.contains('/')); }
                else { assert_eq!(media.filename, *filename); }
                assert_eq!(media.mime_type, *mime_type);
                assert_eq!(media.caption.as_deref(), Some(caption));
                let prepared = media.prepare().unwrap();
                if *mime_type == "image/heic" {
                    PreparedMedia::from_bytes(&request("snapshot.heif", "image/heif", original), original.to_vec()).unwrap();
                }
                let roundtrip = serde_json::to_value(prepared.event_content(MatrixMediaSource::Plain(uri.into()))).unwrap();
                assert_eq!(roundtrip["msgtype"], *msgtype);
                assert_eq!(roundtrip["info"]["mimetype"], *mime_type);
            }
        }
        let requests = server.requests.lock().unwrap();
        let downloads: Vec<_> = requests.iter().filter(|(header, _)| header.contains("/media/download/")).collect();
        assert_eq!(downloads.len(), 10);
        assert!(downloads.iter().all(|(header, _)| header.to_ascii_lowercase().contains("authorization: bearer mock-media-token")));
        assert!(downloads.iter().all(|(header, _)| header.contains("/_matrix/client/v1/media/download/media.test/")));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn native_post_uploads_immutable_bytes_and_requires_send_separately_from_drafting() {
        use a2app_core::information_flow::{self as flow, Recipient, SensitiveAction};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let fixture = MockPermissionContext::new(&server.url);
        let room = OwnedRoomId::try_from("!media:test").unwrap();
        let media = PreparedMedia::from_base64(&request("native.bin", "application/octet-stream", b"reviewed immutable bytes")).unwrap();
        std::fs::write(media.path(), b"different disk contents").unwrap();
        let payload = media.post_payload(&room);
        let send = fixture.auth("matrix.media.send").with_payload(payload.clone());
        let upload = fixture.auth("matrix.media.upload").with_payload(payload.clone());
        let draft = fixture.auth("host.composer.attach").with_payload(payload.clone());
        let before = server.requests.lock().unwrap().len();
        assert!(media.post_with_client(&room, &[draft, upload.clone()], client.clone()).await.is_err());
        assert_eq!(server.requests.lock().unwrap().len(), before, "draft permission must not start an upload or send");
        let recipient = Recipient::MatrixRoom { account: fixture.context.account().into(), room: room.to_string() };
        let action = SensitiveAction { kind: "matrix.media.send".into(), target: room.to_string() };
        let review = flow::prepare_effect_for_activation(&fixture.context, send.flow_epoch.unwrap(), Some(&recipient), Some(&action), &payload).unwrap();
        flow::approve_effect_once(&review).unwrap();
        let result: serde_json::Value = serde_json::from_str(&media.post_with_client(&room, &[send, upload], client.clone()).await.unwrap()).unwrap();
        assert_eq!(result["event_id"], "$posted");
        let requests = server.requests.lock().unwrap();
        let upload = requests.iter().find(|(header, _)| header.starts_with("POST ") && header.contains("/media/") && header.contains("/upload")).unwrap();
        assert_eq!(upload.1, b"reviewed immutable bytes");
        assert!(upload.0.to_ascii_lowercase().contains("authorization: bearer mock-media-token"));
        let sent = requests.iter().find(|(header, _)| header.starts_with("PUT ") && header.contains("/send/m.room.message/")).unwrap();
        let content: serde_json::Value = serde_json::from_slice(&sent.1).unwrap();
        assert_eq!(content["msgtype"], "m.file");
        assert_eq!(content["url"], "mxc://media.test/uploaded");
        assert_eq!(content["filename"], "native.bin");
        assert_eq!(content["body"], "Agent caption");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn encrypted_sdk_upload_uses_ciphertext_and_preserves_decryptable_native_source() {
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let server = MockHomeserver::new();
        let client = server.client().await;
        let fixture = MockPermissionContext::new(&server.url);
        let media = PreparedMedia::from_base64(&request("private.pdf", "application/pdf", include_bytes!("../../../a2app/agent/tests/fixtures/report.pdf"))).unwrap();
        let auth = fixture.auth("matrix.media.upload");
        let source = policy::with_authorization(auth.clone(), async {
            policy::ensure_server_output_with_payload(&server.url, &media.metadata()).await.unwrap();
            media.upload_source(client.clone(), true, RequestConfig::new().retry_limit(0), || auth.check_current_permission()).await.unwrap()
        }).await;
        let MatrixMediaSource::Encrypted(file) = &source else { panic!("the encrypted upload must return its actual encryption descriptor") };
        let requests = server.requests.lock().unwrap();
        let upload = requests.iter().find(|(header, _)| header.starts_with("POST ") && header.contains("/media/") && header.contains("/upload")).unwrap();
        assert_ne!(upload.1, media.bytes());
        assert!(upload.0.to_ascii_lowercase().contains("authorization: bearer mock-media-token"));
        assert_eq!(decrypt_media(upload.1.clone(), (*file.clone()).into()).unwrap(), media.bytes());
        let content = serde_json::to_value(media.event_content(source)).unwrap();
        assert_eq!(content["msgtype"], "m.file");
        assert_eq!(content["file"]["url"], "mxc://media.test/uploaded");
        assert!(content.get("url").is_none());
        assert_eq!(content["info"]["mimetype"], "application/pdf");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn actual_sdk_posts_images_audio_video_and_files_in_plain_and_encrypted_rooms() {
        use a2app_core::information_flow::{self as flow, Recipient, SensitiveAction};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let samples: &[(&str, &str, &str, &[u8])] = &[
            ("m.image", "picture.png", "image/png", include_bytes!("../../../resources/icon_32.png")),
            ("m.audio", "tone.wav", "audio/wav", include_bytes!("../../../a2app/agent/tests/fixtures/tone.wav")),
            ("m.video", "blue.mp4", "video/mp4", include_bytes!("../../../a2app/agent/tests/fixtures/blue.mp4")),
            ("m.file", "report.pdf", "application/pdf", include_bytes!("../../../a2app/agent/tests/fixtures/report.pdf")),
            ("m.image", "snapshot.heic", "image/heic", include_bytes!("../../../a2app/agent/tests/fixtures/vision-colors.heic")),
        ];
        for encrypted in [false, true] {
            let server = MockHomeserver::new();
            server.encrypted.store(encrypted, std::sync::atomic::Ordering::Release);
            let client = server.client().await;
            let fixture = MockPermissionContext::new(&server.url);
            let room_id = OwnedRoomId::try_from("!media:test").unwrap();
            let room = client.get_room(&room_id).unwrap();
            assert_eq!(room.encryption_state().is_encrypted(), encrypted);
            for (msgtype, filename, mime_type, original) in samples {
                let media = PreparedMedia::from_bytes(&request(filename, mime_type, original), original.to_vec()).unwrap();
                let payload = media.post_payload(&room_id);
                let send = fixture.auth("matrix.media.send").with_payload(payload.clone());
                let upload = fixture.auth("matrix.media.upload").with_payload(payload.clone());
                let recipient = Recipient::MatrixRoom { account: fixture.context.account().into(), room: room_id.to_string() };
                let action = SensitiveAction { kind: "matrix.media.send".into(), target: room_id.to_string() };
                let review = flow::prepare_effect_for_activation(&fixture.context, send.flow_epoch.unwrap(), Some(&recipient), Some(&action), &payload).unwrap();
                flow::approve_effect_once(&review).unwrap();
                let before = server.requests.lock().unwrap().len();
                let result = tokio::time::timeout(std::time::Duration::from_secs(15),
                    media.post_with_client(&room_id, &[send, upload], client.clone())).await.expect("native post must complete against the mock homeserver").unwrap();
                assert_eq!(serde_json::from_str::<serde_json::Value>(&result).unwrap()["event_id"], "$posted");
                let (upload_bytes, event_content) = {
                    let captured = server.requests.lock().unwrap();
                    let calls = &captured[before..];
                    let upload = calls.iter().find(|(header, _)| header.starts_with("POST ") && header.contains("/media/") && header.contains("/upload")).unwrap();
                    let send = calls.iter().find(|(header, _)| header.starts_with("PUT ") && header.contains("/send/m.room.")).unwrap();
                    assert!(upload.0.to_ascii_lowercase().contains("authorization: bearer mock-media-token"));
                    assert!(send.0.to_ascii_lowercase().contains("authorization: bearer mock-media-token"));
                    assert_eq!(send.0.contains("/send/m.room.encrypted/"), encrypted);
                    (upload.1.clone(), serde_json::from_slice::<serde_json::Value>(&send.1).unwrap())
                };
                server.events.lock().unwrap().insert("posted".into(), serde_json::json!({
                    "type":if encrypted {"m.room.encrypted"} else {"m.room.message"},
                    "room_id":room_id,"event_id":"$posted","sender":"@media:test","origin_server_ts":1,"content":event_content,
                }));
                let posted_id: matrix_sdk::ruma::OwnedEventId = "$posted".try_into().unwrap();
                let event = room.event(&posted_id, Some(RequestConfig::new().retry_limit(0))).await.unwrap();
                let actual: serde_json::Value = serde_json::from_str(event.raw().json().get()).unwrap();
                assert_eq!(actual["content"]["msgtype"], *msgtype, "the SDK must decrypt the sent event back into the original native media type");
                assert_eq!(actual["content"]["filename"], *filename);
                assert_eq!(actual["content"]["info"]["mimetype"], *mime_type);
                if encrypted {
                    assert_ne!(upload_bytes, *original);
                    let file: matrix_sdk::ruma::events::room::EncryptedFile = serde_json::from_value(actual["content"]["file"].clone()).unwrap();
                    assert_eq!(decrypt_media(upload_bytes, file.into()).unwrap(), *original);
                    assert!(actual["content"].get("url").is_none());
                } else {
                    assert_eq!(upload_bytes, *original);
                    assert_eq!(actual["content"]["url"], "mxc://media.test/uploaded");
                }
            }
        }
    }

    #[tokio::test]
    async fn a_revoked_capture_drops_pending_transport_before_timeout() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let alive = Arc::new(AtomicBool::new(true));
        let stopped = alive.clone();
        let revoke = async { tokio::time::sleep(std::time::Duration::from_millis(20)).await; stopped.store(false, Ordering::Release); };
        let request = await_checked(|| if alive.load(Ordering::Acquire) { Ok(()) } else { Err("cancelled activation".into()) },
            std::future::pending::<Result<(), &str>>());
        let (result, _) = tokio::join!(request, revoke);
        assert_eq!(result.unwrap_err(), "cancelled activation");
    }

    #[test]
    fn posting_review_keeps_once_authority_for_upload_and_rejects_changed_influences() {
        use std::{future::Future, task::{Context, Poll, Waker}};
        use a2app_core::information_flow::{self as flow, ContextId, Influence, Recipient, SensitiveAction};
        let _review_lock = crate::a2app::effect_review::TEST_LOCK.lock().unwrap();
        let context = ContextId::Agent { account: "@media-preflight:test".into(), room: "!media-ai:test".into() };
        flow::register_context(&context).unwrap();
        flow::add_influences(&context, [Influence::InternetOrigin("https://image-source.test".into())]).unwrap();
        let target = OwnedRoomId::try_from("!media-target:test").unwrap();
        let media = PreparedMedia::from_base64(&request("report.bin", "application/octet-stream", b"private report")).unwrap();
        let payload = media.post_payload(&target);
        let authorization = MatrixAuthorization::new("media-preflight", "matrix.media.send", context.room(),
            &a2app_core::permissions::PermissionStore::default()).with_flow(context.clone());
        let mut review = Box::pin(review_media_post(&authorization, &target, &payload));
        assert!(review.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
        let mut pending = crate::a2app::effect_review::take_pending();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].review.action.as_ref().unwrap().kind, "matrix.media.send");
        assert_eq!(pending[0].review.action.as_ref().unwrap().target, target.as_str());
        assert!(pending[0].review.payload.contains(&media.sha256));
        pending[0].approve(flow::approve_effect_once).unwrap();
        pending.pop().unwrap().finish(Ok(()));
        assert!(matches!(review.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(()))));
        let epoch = authorization.flow_epoch.unwrap();
        let recipient = Recipient::MatrixRoom { account: context.account().into(), room: target.to_string() };
        let action = SensitiveAction { kind: "matrix.media.send".into(), target: target.to_string() };
        assert!(flow::check_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_ok(),
            "preflight must leave exact once approval for the final send");
        let mut changed = payload.clone();
        changed["media"]["sha256"] = "different attachment".into();
        assert!(flow::check_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &changed).is_err());
        flow::add_influences(&context, [Influence::InternetOrigin("https://changed-source.test".into())]).unwrap();
        assert!(flow::check_effect_for_activation(&context, epoch, Some(&recipient), Some(&action), &payload).is_err());
        flow::remove_context(&context).unwrap();
    }

    #[test]
    fn drafts_bind_metadata_to_bytes_and_clean_up_after_last_owner() {
        let media = Arc::new(PreparedMedia::from_base64(&request("sample.bin", "application/octet-stream", &[0xff, 0, 0x80])).unwrap());
        let path = media.path().to_owned();
        assert_eq!(std::fs::read(&path).unwrap(), media.bytes());
        assert_eq!(media.metadata()["size"], 3);
        assert_eq!(media.metadata()["sha256"], format!("{:x}", Sha256::digest(media.bytes())));
        std::fs::write(&path, b"changed on disk").unwrap();
        assert_eq!(media.bytes(), [0xff, 0, 0x80]);
        let preview_owner = media.clone();
        drop(media);
        assert!(path.exists());
        drop(preview_owner);
        assert!(!path.exists());
    }

    #[test]
    fn invalid_metadata_and_oversized_sources_never_create_a_draft() {
        for filename in ["", ".", "..", "../private.png", "/private.png", "folder\\image.png", "bad\nname"] {
            assert!(PreparedMedia::from_base64(&request(filename, "application/octet-stream", b"file")).is_err());
        }
        for mime_type in ["invalid", "*/*", "image/*"] {
            assert!(PreparedMedia::from_base64(&request("file", mime_type, b"file")).is_err());
        }
        let mut oversized = request("file", "application/octet-stream", b"file");
        oversized.source = MediaSource::Base64("A".repeat(MAX_INLINE_MEDIA_BASE64_BYTES + 1));
        assert!(PreparedMedia::from_base64(&oversized).is_err());
        assert!(PreparedMedia::from_bytes(&oversized, vec![0; crate::a2app::network::MAX_MEDIA_RESPONSE_BODY + 1]).is_err());
        assert!(PreparedMedia::from_base64(&request("encrypted.png", "image/png", b"encrypted ciphertext")).is_err());
    }

    #[test]
    fn media_content_uses_native_types_caption_filename_and_mime() {
        let media = PreparedMedia::from_base64(&request("clip.ogg", "audio/ogg", b"audio bytes")).unwrap();
        let source = MatrixMediaSource::Plain("mxc://example.org/sample".into());
        let content = media.event_content(source);
        let MessageType::Audio(content) = content.msgtype else { panic!("wrong media type") };
        assert_eq!(content.body, "Agent caption");
        assert_eq!(content.filename(), "clip.ogg");
        assert_eq!(content.info.unwrap().mimetype.as_deref(), Some("audio/ogg"));
    }

    #[test]
    fn an_empty_caption_uses_the_filename_as_the_media_event_body() {
        let mut draft = request("fixture.bin", "application/octet-stream", b"file bytes");
        draft.caption = Some(String::new());
        let media = PreparedMedia::from_base64(&draft).unwrap();
        let content = serde_json::to_value(media.event_content(MatrixMediaSource::Plain("mxc://example.org/fixture".into()))).unwrap();
        assert_eq!(content["body"], "fixture.bin");
        assert_eq!(content["filename"], "fixture.bin");
    }

    #[test]
    fn native_image_event_includes_dimensions_and_a_real_media_source() {
        let media = PreparedMedia::from_base64(&request("picture.png", "image/png", include_bytes!("../../../resources/icon_32.png"))).unwrap();
        let source = MatrixMediaSource::Plain("mxc://example.org/picture".into());
        let content = serde_json::to_value(media.event_content(source)).unwrap();
        assert_eq!(content["msgtype"], "m.image");
        assert_eq!(content["url"], "mxc://example.org/picture");
        assert_eq!(content["info"]["w"], 32);
        assert_eq!(content["info"]["h"], 32);
    }

    #[test]
    fn compressed_image_headers_cannot_request_an_oversized_native_preview() {
        let mut bytes = include_bytes!("../../../resources/icon_32.png").to_vec();
        bytes[16..20].copy_from_slice(&8192u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&8192u32.to_be_bytes());
        // Keep the IHDR valid so rejection comes from the preview pixel
        // budget before attempting to decode its compressed body.
        let mut crc = u32::MAX;
        for byte in &bytes[12..29] {
            crc ^= u32::from(*byte);
            for _ in 0..8 { crc = (crc >> 1) ^ (0xedb88320 & 0u32.wrapping_sub(crc & 1)); }
        }
        bytes[29..33].copy_from_slice(&(!crc).to_be_bytes());
        let request = request("large.png", "image/png", &bytes);
        assert!(PreparedMedia::from_base64(&request).unwrap_err().contains("media draft limit"));
    }

    #[test]
    fn encrypted_attachment_decryption_checks_the_original_hash() {
        use std::io::Read;
        let mut original = std::io::Cursor::new(b"private attachment");
        let mut encryptor = matrix_sdk_base::crypto::AttachmentEncryptor::new(&mut original);
        let mut encrypted = Vec::new();
        encryptor.read_to_end(&mut encrypted).unwrap();
        let encryption = encryptor.finish();
        let copy = serde_json::from_value(serde_json::to_value(&encryption).unwrap()).unwrap();
        assert_eq!(decrypt_media(encrypted.clone(), copy).unwrap(), b"private attachment");
        encrypted[0] ^= 1;
        assert!(decrypt_media(encrypted, encryption).unwrap_err().contains("hash"));
    }

    #[test]
    fn cached_uri_resolution_requires_an_original_media_event_in_the_source_room() {
        fn event(room: &str, content: serde_json::Value) -> matrix_sdk::deserialized_responses::TimelineEvent {
            let value = serde_json::json!({"type":"m.room.message","room_id":room,"event_id":"$media",
                "sender":"@sender:example.org","origin_server_ts":1,"content":content});
            matrix_sdk::deserialized_responses::TimelineEvent::from_plaintext(
                matrix_sdk::ruma::serde::Raw::from_json_string(value.to_string()).unwrap())
        }
        let room: OwnedRoomId = "!source:example.org".try_into().unwrap();
        let uri = "mxc://example.org/original";
        let content = serde_json::json!({"msgtype":"m.image","body":"original.png","url":uri,"info":{"size":12}});
        let original = event(room.as_str(), content.clone());
        let matching = resolve_cached_media_uri(uri, &room, &[original]).unwrap();
        assert!(matches!(matching.0, MatrixMediaSource::Plain(_)));
        assert_eq!(matching.1.unwrap(), matrix_sdk::ruma::UInt::from(12u32));
        assert!(resolve_cached_media_uri(uri, &room, &[event("!other:example.org", content)]).is_none());
        let text = event(room.as_str(), serde_json::json!({"msgtype":"m.text","body":uri,"url":uri}));
        assert!(resolve_cached_media_uri(uri, &room, &[text]).is_none());
        assert!(resolve_cached_media_uri("mxc://example.org/invented", &room, &[]).is_none());
    }
}
