//! Wallpaper profile creation: slices a gridded image into one tile per physical
//! keypad button and generates a profile whose every key displays its tile.

use super::Error;

use crate::shared::{Action, ActionContext, ActionInstance, ActionState, CATEGORIES, DEVICES, config_dir};
use crate::store::profiles::{acquire_locks_mut, get_device_profiles};

use base64::Engine;
use tauri::command;

const RUN_COMMAND_UUID: &str = "com.amansprojects.starterpack.runcommand";
const KEY_IMAGE_SIZE: u32 = 72;
const KEY_SEPARATOR: u32 = 16;

/// Computes the crop rectangle (x, y, width, height) of every tile, left-to-right
/// and top-to-bottom, matching the device's keypad layout.
///
/// Two layouts are supported:
/// - separated grids: `KEY_IMAGE_SIZE`px tiles with `KEY_SEPARATOR`px divider strips
///   between them (e.g. 424×248px for a 5×3 deck), and
/// - seamless grids: any size evenly divisible into `columns × rows` tiles.
pub fn tile_rects(width: u32, height: u32, columns: u32, rows: u32) -> Result<Vec<(u32, u32, u32, u32)>, anyhow::Error> {
	if columns == 0 || rows == 0 {
		anyhow::bail!("device has no keypad buttons");
	}
	let tiles = |tile_width: u32, tile_height: u32, stride: (u32, u32)| -> Vec<(u32, u32, u32, u32)> {
		let mut rects = Vec::with_capacity((columns * rows) as usize);
		for row in 0..rows {
			for column in 0..columns {
				rects.push((column * stride.0, row * stride.1, tile_width, tile_height));
			}
		}
		rects
	};

	let separated_size = (
		columns * KEY_IMAGE_SIZE + (columns - 1) * KEY_SEPARATOR,
		rows * KEY_IMAGE_SIZE + (rows - 1) * KEY_SEPARATOR,
	);
	if (width, height) == separated_size {
		// Divider strips between the tiles, as in a rendered grid preview.
		return Ok(tiles(KEY_IMAGE_SIZE, KEY_IMAGE_SIZE, (KEY_IMAGE_SIZE + KEY_SEPARATOR, KEY_IMAGE_SIZE + KEY_SEPARATOR)));
	}
	if width % columns == 0 && height % rows == 0 {
		// A seamless grid whose size divides evenly into the tiles.
		let cell = (width / columns, height / rows);
		return Ok(tiles(cell.0, cell.1, cell));
	}
	anyhow::bail!(
		"the image is {width}×{height}px, which fits neither a {}×{}px separated grid (tile×strip layout) nor a size divisible into {columns}×{rows} tiles",
		separated_size.0,
		separated_size.1
	)
}

/// Extracts the image bytes from a base64 data URL attachment.
fn decode_data_url(image: &str) -> Result<Vec<u8>, anyhow::Error> {
	let Some((metadata, data)) = image.split_once(";base64,") else {
		anyhow::bail!("the attachment is not a base64 image data URL");
	};
	if !metadata.starts_with("data:image/") {
		anyhow::bail!("the attachment is not an image data URL ({metadata})");
	}
	base64::engine::general_purpose::STANDARD.decode(data).map_err(|error| anyhow::anyhow!("failed to decode the attached image: {error}"))
}

/// Creates a profile whose every keypad button shows one tile of `image`.
#[command]
pub async fn create_wallpaper_profile(device: String, name: String, image: String) -> Result<(), Error> {
	let device_info = {
		let Some(device_info) = DEVICES.get(&device) else {
			return Err(Error::new(format!("device {device} not found")));
		};
		device_info.value().clone()
	};

	let name = name.trim();
	if name.is_empty() {
		return Err(Error::new("the profile name must not be empty".to_owned()));
	}
	if name.contains('/') {
		return Err(Error::new("the profile name must not contain '/'".to_owned()));
	}
	if get_device_profiles(&device)?.iter().any(|id| id == name) {
		return Err(Error::new(format!("a profile named {name:?} already exists")));
	}

	let bytes = decode_data_url(image.as_str()).map_err(Error::from)?;
	let image = image::load_from_memory(&bytes).map_err(|error| Error::new(format!("failed to read the attached image: {error}")))?;
	let rects = tile_rects(image.width(), image.height(), device_info.columns as u32, device_info.rows as u32).map_err(Error::from)?;

	// The Starter Pack "Run Command" action: the same inert placeholder used by the
	// bundled profiles, so the buttons stay pressable and survive instance pruning.
	let action: Action = {
		let categories = CATEGORIES.read().await;
		let run_command = categories.values().flat_map(|category| category.actions.iter()).find(|action| action.uuid == RUN_COMMAND_UUID).cloned();
		run_command.ok_or_else(|| Error::new("the Run Command action was not found; is the Starter Pack plugin installed?".to_owned()))?
	};

	let rgba = image.to_rgba8();
	let tiles_root = config_dir().join("images").join(&device).join(name);
	for (position, (x, y, width, height)) in rects.iter().enumerate() {
		let directory = tiles_root.join(format!("Keypad.{position}.0"));
		std::fs::create_dir_all(&directory).map_err(Error::from)?;
		let tile = image::imageops::crop_imm(&rgba, *x, *y, *width, *height).to_image();
		tile.save(directory.join("0.png")).map_err(|error| Error::new(format!("failed to write a button tile: {error}")))?;
	}

	let mut locks = acquire_locks_mut().await;
	let store = locks.profile_stores.get_profile_store_mut(&device_info, name).await?;
	for position in 0..rects.len() {
		let instance = ActionInstance {
			action: action.clone(),
			context: ActionContext {
				device: device.to_owned(),
				profile: name.to_owned(),
				controller: "Keypad".to_owned(),
				position: position as u8,
				index: 0,
			},
			states: vec![ActionState {
				image: tiles_root.join(format!("Keypad.{position}.0")).join("0.png").to_string_lossy().into_owned(),
				..Default::default()
			}],
			current_state: 0,
			settings: serde_json::json!({ "command": "true" }),
			children: None,
		};
		store.value.keys[position] = Some(instance);
	}
	store.save()?;

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::tile_rects;

	#[test]
	fn slices_a_separated_grid_row_major() {
		let rects = tile_rects(424, 248, 5, 3).unwrap();
		assert_eq!(rects.len(), 15);
		assert_eq!(rects[0], (0, 0, 72, 72));
		assert_eq!(rects[4], (352, 0, 72, 72));
		assert_eq!(rects[5], (0, 88, 72, 72));
		assert_eq!(rects[7], (176, 88, 72, 72));
		assert_eq!(rects[14], (352, 176, 72, 72));
	}

	#[test]
	fn slices_a_seamless_grid() {
		let rects = tile_rects(360, 216, 5, 3).unwrap();
		assert_eq!(rects.len(), 15);
		assert_eq!(rects[0], (0, 0, 72, 72));
		assert_eq!(rects[6], (72, 72, 72, 72));
		assert_eq!(rects[14], (288, 144, 72, 72));
	}

	#[test]
	fn slices_a_seamless_grid_with_oversized_tiles() {
		let rects = tile_rects(500, 300, 5, 3).unwrap();
		assert_eq!(rects.len(), 15);
		assert_eq!(rects[2], (200, 0, 100, 100));
		assert_eq!(rects[8], (300, 100, 100, 100));
	}

	#[test]
	fn rejects_an_unslicable_image() {
		assert!(tile_rects(421, 247, 5, 3).is_err());
		assert!(tile_rects(360, 220, 5, 3).is_err());
	}
}
