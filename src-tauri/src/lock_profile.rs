use std::collections::HashMap;
use std::sync::{LazyLock, RwLock};

use crate::store::profiles::DEVICE_STORES;

static PROFILE_WHEN_LOCKED: LazyLock<RwLock<String>> = LazyLock::new(|| RwLock::new(String::new()));
static PREVIOUS_PROFILES: LazyLock<RwLock<HashMap<String, PreviousProfile>>> = LazyLock::new(|| RwLock::new(HashMap::new()));

struct PreviousProfile {
	applied: String,
	previous: String,
}

pub fn init_lock_profile(initial: String) {
	*(PROFILE_WHEN_LOCKED.write().unwrap()) = initial;
}

pub fn update_profile_when_locked(profile: String) {
	*(PROFILE_WHEN_LOCKED.write().unwrap()) = profile;
}

/// Applies the configured profile to all devices when the computer locks.
///
/// Safe to call when the feature is disabled (empty profile) or when a device
/// does not have the profile installed: those devices are skipped unchanged
/// instead of having a blank profile created for them.
pub async fn apply_profile_for_computer_lock() {
	let profile = PROFILE_WHEN_LOCKED.read().unwrap().clone();
	if profile.is_empty() {
		return;
	}

	let device_ids = crate::shared::DEVICES.iter().map(|entry| entry.id.clone()).collect::<Vec<_>>();
	for device in device_ids {
		let installed = match crate::store::profiles::get_device_profiles(&device) {
			Ok(installed) => installed,
			Err(error) => {
				log::warn!("Failed to list profiles for device {device}: {error}");
				continue;
			}
		};
		if !installed.contains(&profile) {
			continue;
		}

		let previous = match DEVICE_STORES.write().await.get_selected_profile(&device) {
			Ok(previous) => previous,
			Err(error) => {
				log::warn!("Failed to get the selected profile for device {device}: {error}");
				continue;
			}
		};
		if previous == profile {
			continue;
		}

		// The snapshot is only taken on success, so a failed application does
		// not let a later unlock overwrite whatever state the device reached.
		if let Err(error) = crate::events::frontend::profiles::set_selected_profile(device.clone(), profile.clone()).await {
			log::error!("Failed to apply profile {profile:?} to device {device} after lock: {error}");
			continue;
		}
		render_profile_images(&device, &profile).await;
		PREVIOUS_PROFILES.write().unwrap().insert(device, PreviousProfile { applied: profile.clone(), previous });
	}
}

/// Draws stored state images server-side; the webview renderer adds nothing while the screen is locked.
async fn render_profile_images(device: &str, profile: &str) {
	let locks = crate::store::profiles::acquire_locks().await;
	let Some(entry) = crate::shared::DEVICES.get(device) else { return };
	let Ok(store) = locks.profile_stores.get_profile_store(&entry, profile) else { return };
	for instance in store
		.value
		.keys
		.iter()
		.flatten()
		.chain(store.value.sliders.iter().flatten())
		.chain(store.value.infobars.iter().flatten())
	{
		if matches!(instance.action.uuid.as_str(), "opendeck.multiaction" | "opendeck.toggleaction") {
			continue;
		}

		let state = instance.states.get(instance.current_state as usize).or_else(|| instance.states.first());
		let Some(state) = state else { continue };

		let image = if state.image.is_empty() { None } else { Some(state.image.clone()) };
		if let Err(error) = crate::events::outbound::devices::update_image((&instance.context).into(), image, Some(state.background_colour.clone()), Some(state.image_scale)).await {
			log::warn!("Failed to render instance to device at lock: {}", error);
		}
	}
}

/// Returns devices to their previous profiles after the computer unlocks.
///
/// Skips devices whose selected profile no longer matches what the lock applied, preserving later changes.
pub async fn restore_profile_after_computer_unlock() {
	let snapshot = PREVIOUS_PROFILES.write().unwrap().drain().collect::<Vec<_>>();
	for (device, PreviousProfile { applied, previous }) in snapshot {
		let current = match DEVICE_STORES.write().await.get_selected_profile(&device) {
			Ok(current) => current,
			Err(error) => {
				log::warn!("Failed to get the selected profile for device {device}: {error}");
				continue;
			}
		};
		if current != applied {
			continue;
		}

		if let Err(error) = crate::events::frontend::profiles::set_selected_profile(device.clone(), previous.clone()).await {
			log::error!("Failed to restore profile for device {device} after unlock: {error}");
			continue;
		}
		render_profile_images(&device, &previous).await;
	}
}
