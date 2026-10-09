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
            let (width, height) = image_size_by_data(&bytes, Path::new(&request.filename))
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
        let primary = authorizations.first().ok_or("Missing media-send authorization.")?.clone();
        policy::with_authorization(primary.clone(), async {
            let check_permissions = || -> Result<(), String> {
                for authorization in authorizations { authorization.check_current_permission()?; }
                policy::ensure_room_access(target.as_str(), RoomAccess::Write)
            };
            check_permissions()?;
            let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
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
            let encrypted = policy::audit_server_operation(homeserver.as_str(), room.latest_encryption_state()).await
                .map_err(|error| format!("Could not check room encryption: {error}"))?.is_encrypted();
            check()?;
            policy::ensure_server_output_with_payload(homeserver.as_str(), &payload).await?;
            check()?;
            let max_size = policy::audit_server_operation(homeserver.as_str(), client.load_or_fetch_max_upload_size()).await
                .map_err(|error| format!("Could not check the homeserver upload limit: {error}"))?;
            if self.bytes.len() as u64 > u64::from(max_size) { return Err("The media exceeds the homeserver upload limit.".into()); }
            check()?;

            policy::ensure_server_output_with_payload(homeserver.as_str(), &payload).await?;
            check()?;
            let request_config = RequestConfig::new().retry_limit(0);
            // Encryption may have been enabled while metadata or approval
            // was pending. Never ignore a newer encrypted SDK room state.
            let encrypted = encrypted || room.encryption_state().is_encrypted();
            let source = if encrypted {
                let mut reader = std::io::Cursor::new(self.bytes.to_vec());
                let file = policy::audit_server_operation(homeserver.as_str(),
                    client.upload_encrypted_file(&mut reader).with_request_config(request_config)).await
                    .map_err(|error| format!("Could not upload encrypted media: {error}"))?;
                MatrixMediaSource::Encrypted(Box::new(file))
            } else {
                let uploaded = policy::audit_server_operation(homeserver.as_str(),
                    client.media().upload(&self.mime_type, self.bytes.to_vec(), Some(request_config))).await
                    .map_err(|error| format!("Could not upload media: {error}"))?;
                MatrixMediaSource::Plain(uploaded.content_uri)
            };
            check()?;
            if matches!(&source, MatrixMediaSource::Plain(_)) && room.encryption_state().is_encrypted() {
                return Err("The room enabled encryption during the upload. Post this draft again to upload encrypted media.".into());
            }
            let content = self.event_content(source);
            policy::commit_sensitive_target(target.as_str(), &payload).await?;
            check_permissions()?;
            let sent = policy::audit_room_operation(target.as_str(),
                room.send(content).with_request_config(RequestConfig::new().retry_limit(0))).await
                .map_err(|error| format!("Could not post media to the room: {error}"))?;
            Ok(serde_json::json!({"room_id":target,"event_id":sent.response.event_id,"media":self.metadata()}).to_string())
        }).await
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
        download_descriptor(descriptor, authorization).await
    }).await
}

/// Download plaintext MXC media through the logged-in account's own
/// authenticated homeserver endpoint. A dedicated client prevents redirects
/// from forwarding the access token and bounds allocation while streaming.
async fn raw_download_mxc(uri: &str, authorization: MatrixAuthorization) -> Result<Vec<u8>, String> {
    policy::with_authorization(authorization.clone(), async {
        authorization.check_current_permission()?;
        let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
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
        let mut response = policy::audit_server_operation(client.homeserver().as_str(),
            http.get(destination).bearer_auth(token).header(matrix_sdk::reqwest::header::ACCEPT_ENCODING, "identity").send()).await
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
            let chunk = response.chunk().await.map_err(|_| "Reading Matrix media failed.".to_string())?;
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
    use matrix_sdk::ruma::OwnedEventId;
    policy::with_authorization(authorization.clone(), async {
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let event_id = OwnedEventId::try_from(event_id).map_err(|_| "The media event ID is invalid.")?;
        let client = crate::sliding_sync::get_client().ok_or("Not logged in.")?;
        let room = client.get_room(source_room).ok_or("The media source room was not found.")?;
        if room.state() != matrix_sdk::RoomState::Joined { return Err("The media source room is not joined.".into()); }
        let payload = serde_json::json!({"operation":"matrix.media.download","room_id":source_room,"event_id":event_id});
        policy::ensure_server_output_with_payload(client.homeserver().as_str(), &payload).await?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let event = policy::audit_server_operation(client.homeserver().as_str(),
            room.event(&event_id, Some(RequestConfig::new().retry_limit(0)))).await
            .map_err(|error| format!("Could not fetch the media event: {error}"))?;
        authorization.check_current_permission()?;
        policy::ensure_room_access(source_room.as_str(), RoomAccess::Read)?;
        let (actual_event_id, source, size) = media_source_from_event(&event, source_room)
            .ok_or("The selected event is not an available media message in the source room.")?;
        if actual_event_id != event_id { return Err("The homeserver returned a different media event.".into()); }
        download_descriptor((source, size), authorization).await
    }).await
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
) -> Result<Vec<u8>, String> {
    let (source, size) = descriptor;
    if size.is_some_and(|size| u64::from(size) > crate::a2app::network::MAX_MEDIA_RESPONSE_BODY as u64) {
        return Err("The Matrix media exceeds the 16 MiB download limit.".into());
    }
    let uri = match &source {
        MatrixMediaSource::Plain(uri) => uri,
        MatrixMediaSource::Encrypted(file) => &file.url,
    };
    let bytes = raw_download_mxc(uri.as_str(), authorization.clone()).await?;
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
