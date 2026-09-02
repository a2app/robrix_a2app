//! Account-scoped services: the user's own identity and settings.

use matrix_sdk::ruma::OwnedUserId;
use matrix_sdk::ruma::api::client::discovery::get_authorization_server_metadata::v1::{AccountManagementAction, AccountManagementActionData};
use matrix_sdk::ruma::api::client::profile::{AvatarUrl, DisplayName};
use matrix_sdk::ruma::events::ignored_user_list::IgnoredUserListEventContent;

use super::rooms::room_name;
use crate::sliding_sync::{current_user_id, get_client, is_user_ignored};

pub(super) async fn user_profile(user_id: OwnedUserId) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let profile = client.account().fetch_user_profile_of(&user_id).await
        .map_err(|e| format!("couldn't fetch the profile: {e}"))?;
    let display_name = profile.get_static::<DisplayName>().ok().flatten()
        .unwrap_or_else(|| user_id.localpart().to_string());
    Ok(serde_json::json!({
        "user_id": user_id,
        "display_name": display_name,
        "has_avatar": profile.get_static::<AvatarUrl>().ok().flatten().is_some(),
        "ignored": is_user_ignored(&user_id),
    }).to_string())
}

pub(super) async fn dm_find(user_id: OwnedUserId) -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let Some(room) = client.get_dm_room(&user_id) else {
        return Ok(serde_json::json!({ "room_id": null, "name": null }).to_string());
    };
    Ok(serde_json::json!({
        "room_id": room.room_id(),
        "name": room_name(&room).await,
    }).to_string())
}

pub(super) async fn device() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let device_id = client.device_id().ok_or("not logged in")?.to_owned();
    let device = client.encryption().get_own_device().await
        .map_err(|e| format!("couldn't load this device: {e}"))?;
    Ok(serde_json::json!({
        "device_id": device_id,
        "name": device.as_ref().and_then(|d| d.display_name()),
        "verified": device.as_ref().is_some_and(|d| d.is_verified()),
    }).to_string())
}

pub(super) async fn info() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let user_id = current_user_id().ok_or("not logged in")?;
    // Only homeservers on OAuth have an account management page.
    let account_management_url = match client.oauth().cached_server_metadata().await {
        Ok(md) if md.is_account_management_action_supported(&AccountManagementAction::Profile) =>
            md.account_management_url_with_action(AccountManagementActionData::Profile),
        Ok(md) => md.account_management_uri,
        Err(_) => None,
    };
    Ok(serde_json::json!({
        "user_id": user_id,
        "homeserver": client.homeserver().to_string(),
        "account_management_url": account_management_url.map(|u| u.to_string()),
    }).to_string())
}

pub(super) async fn ignored_users() -> Result<String, String> {
    let client = get_client().ok_or("not logged in")?;
    let raw = client.account().account_data::<IgnoredUserListEventContent>().await
        .map_err(|e| format!("couldn't read the ignore list: {e}"))?;
    let users: Vec<OwnedUserId> = match raw {
        Some(raw) => raw.deserialize()
            .map_err(|e| format!("couldn't read the ignore list: {e}"))?
            .ignored_users.into_keys().collect(),
        None => Vec::new(),
    };
    Ok(serde_json::json!({ "users": users }).to_string())
}
