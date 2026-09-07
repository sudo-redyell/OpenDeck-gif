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
		PREVIOUS_PROFILES.write().unwrap().insert(device, PreviousProfile { applied: profile.clone(), previous });
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

		if let Err(error) = crate::events::frontend::profiles::set_selected_profile(device.clone(), previous).await {
			log::error!("Failed to restore profile for device {device} after unlock: {error}");
		}
	}
}
