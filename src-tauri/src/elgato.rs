use crate::encoder_layouts::generate_encoder_image;
use crate::events::inbound;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use dashmap::DashMap;
use elgato_streamdeck::{
	AsyncStreamDeck, DeviceStateUpdate,
	images::{ImageRect, convert_image_with_format_async},
	info::{ImageRotation, Kind},
};
use image::GenericImageView as _;
use tokio::sync::RwLock;

static ELGATO_DEVICES: LazyLock<RwLock<HashMap<String, AsyncStreamDeck>>> = LazyLock::new(|| RwLock::new(HashMap::new()));
static HIDAPI: LazyLock<RwLock<Option<Arc<hidapi::HidApi>>>> = LazyLock::new(|| RwLock::new(None));

/// Running button animation loops, keyed by `device/controller/position`.
/// Values carry a generation counter so a finished task only ever removes its
/// own entry, and an abort handle for replacement.
static ANIMATION_TASKS: LazyLock<DashMap<String, (u64, Option<tokio::task::AbortHandle>)>> = LazyLock::new(DashMap::new);
static ANIMATION_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Extract the average colour from an image.
fn extract_average_colour(img: &image::DynamicImage) -> (u8, u8, u8) {
	let (r_sum, g_sum, b_sum) = img
		.pixels()
		.fold((0u64, 0u64, 0u64), |(r, g, b), (_, _, pixel)| (r + pixel[0] as u64, g + pixel[1] as u64, b + pixel[2] as u64));
	let count = (img.width() * img.height()).max(1) as u64;
	((r_sum / count) as u8, (g_sum / count) as u8, (b_sum / count) as u8)
}

pub async fn update_image(context: &crate::shared::Context, image: Option<&str>, background: Option<&str>, image_scale: Option<u8>) -> Result<(), anyhow::Error> {
	if let Some(device) = ELGATO_DEVICES.read().await.get(&context.device) {
		let kind = device.kind();
		if !kind.is_visual() {
			return Ok(());
		}
		let key_count = kind.key_count();
		let is_touch_point = context.controller == "Keypad" && context.position >= key_count;

		if let Some(image) = image {
			let bytes = if image.starts_with("data:") {
				crate::gif_animation::extract_base64_payload(image).ok_or_else(|| anyhow::anyhow!("Unsupported image payload; expected a base64 data URL"))?
			} else {
				// Profile images stay absolute paths inside the image store.
				crate::shared::read_config_image(image).map_err(anyhow::Error::msg)?
			};
			if context.controller == "Encoder" {
				let mut img = generate_encoder_image(context, &bytes).await?;
				let Some(format) = device.kind().lcd_image_format() else {
					return Err(anyhow::anyhow!("Failed to get LCD image format"));
				};
				img = match format.rotation {
					ImageRotation::Rot0 => img,
					ImageRotation::Rot90 => img.rotate90(),
					ImageRotation::Rot180 => img.rotate180(),
					ImageRotation::Rot270 => img.rotate270(),
				};
				device.write_lcd(context.position as u16 * 200, 0, &ImageRect::from_image_async(img)?).await?;
			} else if context.controller == "Infobar" {
				let img = image::load_from_memory(&bytes)?;
				let Some(format) = device.kind().lcd_image_format() else {
					return Err(anyhow::anyhow!("Failed to get LCD image format"));
				};
				let data = convert_image_with_format_async(format, img.resize_exact(248, 58, image::imageops::FilterType::Lanczos3))?;
				device.write_lcd_fill(&data).await?;
			} else if is_touch_point {
				let (r, g, b) = extract_average_colour(&image::load_from_memory(&bytes)?);
				device.set_touchpoint_color(context.position - key_count, r, g, b).await?;
			} else {
				// Regular buttons stream animated GIFs frame by frame; a single
				// frame (or any other format) follows the static path.
				if let Some(animation) = crate::gif_animation::decode_animated_gif(&bytes).map_err(anyhow::Error::msg)? {
					start_button_animation(context, animation, background, image_scale).await?;
					return Ok(());
				}
				cancel_button_animation(context);
				device.set_button_image(context.position, image::load_from_memory(&bytes)?).await?;
			}
		} else if context.controller == "Encoder" {
			let mut img = image::DynamicImage::new_rgb8(200, 100);
			let Some(format) = device.kind().lcd_image_format() else {
				return Err(anyhow::anyhow!("Failed to get LCD image format"));
			};
			img = match format.rotation {
				ImageRotation::Rot0 => img,
				ImageRotation::Rot90 => img.rotate90(),
				ImageRotation::Rot180 => img.rotate180(),
				ImageRotation::Rot270 => img.rotate270(),
			};
			device.write_lcd(context.position as u16 * 200, 0, &ImageRect::from_image_async(img)?).await?;
		} else if context.controller == "Infobar" {
			let Some(format) = device.kind().lcd_image_format() else {
				return Err(anyhow::anyhow!("Failed to get LCD image format"));
			};
			let data = convert_image_with_format_async(format, image::DynamicImage::new_rgb8(248, 58))?;
			device.write_lcd_fill(&data).await?;
		} else if is_touch_point {
			device.set_touchpoint_color(context.position - key_count, 0, 0, 0).await?;
		} else {
			cancel_button_animation(context);
			device.clear_button_image(context.position).await?;
		}
		device.flush().await?;
	}
	Ok(())
}

/// Clear all touchpoint LEDs on a device by setting them to black.
async fn clear_all_touchpoints(device: &AsyncStreamDeck) {
	for i in 0..device.kind().touchpoint_count() {
		let _ = device.set_touchpoint_color(i, 0, 0, 0).await;
	}
}

pub async fn clear_screen(id: &str) -> Result<(), anyhow::Error> {
	cancel_device_animations(id);
	if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
		device.clear_all_button_images().await?;
		if let Some(lcd_format) = device.kind().lcd_image_format() {
			device
				.write_lcd_fill(&convert_image_with_format_async(
					lcd_format,
					image::DynamicImage::new_rgb8(lcd_format.size.0 as u32, lcd_format.size.1 as u32),
				)?)
				.await?;
		}
		clear_all_touchpoints(device).await;
		device.flush().await?;
	}
	Ok(())
}

pub async fn set_brightness(id: &str, brightness: u8) {
	if let Some(device) = ELGATO_DEVICES.read().await.get(id) {
		let _ = device.set_brightness(brightness.clamp(0, 100)).await;
		let _ = device.flush().await;
	}
}

pub async fn reset_devices() {
	for (_id, device) in ELGATO_DEVICES.read().await.iter() {
		let _ = device.reset().await;
		let _ = device.flush().await;
	}
}

fn animation_key(context: &crate::shared::Context) -> String {
	format!("{}/{}/{}", context.device, context.controller, context.position)
}

fn cancel_button_animation(context: &crate::shared::Context) {
	if let Some((_, task)) = ANIMATION_TASKS.remove(&animation_key(context))
		&& let Some(handle) = task.1
	{
		handle.abort();
	}
}

/// Cancels every running button animation for a device.
pub fn cancel_device_animations(device: &str) {
	let prefix = format!("{device}/");
	let keys: Vec<String> = ANIMATION_TASKS.iter().filter(|entry| entry.key().starts_with(&prefix)).map(|entry| entry.key().clone()).collect();
	for key in keys {
		if let Some((_, task)) = ANIMATION_TASKS.remove(&key)
			&& let Some(handle) = task.1
		{
			handle.abort();
		}
	}
}

async fn start_button_animation(context: &crate::shared::Context, animation: crate::gif_animation::AnimatedImage, background: Option<&str>, image_scale: Option<u8>) -> Result<(), anyhow::Error> {
	let Some(device) = ELGATO_DEVICES.read().await.get(&context.device).cloned() else {
		return Ok(());
	};

	let frames = animation
		.frames
		.iter()
		.map(|frame| image::DynamicImage::ImageRgba8(crate::gif_animation::compose_frame(frame, background, image_scale)))
		.collect::<Vec<_>>();
	let delays = animation.delays.clone();

	let generation = ANIMATION_GENERATION.fetch_add(1, Ordering::Relaxed);

	// The registry entry lock spans abort + replace so two tasks never write one button.
	let previous_handle = {
		let mut entry = ANIMATION_TASKS.entry(animation_key(context)).or_insert((u64::MAX, None));
		let previous_handle = entry.1.take();
		let handle = tokio::spawn(run_button_animation(
			device,
			context.position,
			context.device.clone(),
			animation_key(context),
			generation,
			frames,
			delays,
		))
		.abort_handle();
		*entry = (generation, Some(handle));
		previous_handle
	};
	if let Some(previous_handle) = previous_handle {
		previous_handle.abort();
	}
	Ok(())
}

async fn run_button_animation(device: AsyncStreamDeck, position: u8, device_id: String, key: String, generation: u64, frames: Vec<image::DynamicImage>, delays: Vec<Duration>) {
	loop {
		for (frame, delay) in frames.iter().zip(delays.iter()) {
			// Skip writes while the device sleeps (brightness 0), keeping the loop ticking.
			if !crate::device_sleep::is_device_sleeping(&device_id) {
				let result = match device.set_button_image(position, frame.clone()).await {
					Ok(()) => device.flush().await,
					Err(error) => Err(error),
				};
				if let Err(error) = result {
					log::warn!("Stopped button animation for {key}: {error}");
					ANIMATION_TASKS.remove_if(&key, |_, (running, _)| *running == generation);
					return;
				}
			}
			tokio::time::sleep(*delay).await;
		}
	}
}

async fn init(device: AsyncStreamDeck, device_id: String) {
	if ELGATO_DEVICES.read().await.contains_key(&device_id) {
		return;
	}

	let device_name = device.product().await.unwrap();
	let kind = device.kind();
	let device_type = match kind {
		Kind::Original | Kind::OriginalV2 | Kind::Mk2 | Kind::Mk2Scissor | Kind::Mk2Module => 0,
		Kind::Mini | Kind::MiniMk2 | Kind::MiniDiscord | Kind::MiniMk2Module => 1,
		Kind::Xl | Kind::XlV2 | Kind::XlV2Module => 2,
		Kind::Pedal => 5,
		Kind::Plus => 7,
		Kind::Neo => 9,
		Kind::PlusXl => 13,
	};
	let _ = device.clear_all_button_images().await;
	clear_all_touchpoints(&device).await;
	let _ = device.set_brightness(crate::store::get_settings().value.brightness).await;
	let _ = device.flush().await;

	let reader = device.get_reader();
	ELGATO_DEVICES.write().await.insert(device_id.clone(), device);
	let _ = clear_screen(&device_id).await;

	crate::events::inbound::devices::register_device(
		"",
		crate::events::inbound::PayloadEvent {
			payload: crate::shared::DeviceInfo {
				id: device_id.clone(),
				plugin: String::new(),
				name: device_name,
				rows: kind.row_count(),
				columns: kind.column_count(),
				encoders: kind.encoder_count(),
				touchpoints: kind.touchpoint_count(),
				infobars: if kind == Kind::Neo { 1 } else { 0 },
				r#type: device_type,
			},
		},
	)
	.await
	.unwrap();

	let press = |position| inbound::PayloadEvent {
		payload: inbound::devices::PressPayload { device: device_id.clone(), position },
	};
	let encoder = |position, ticks: i8| inbound::PayloadEvent {
		payload: inbound::devices::TicksPayload {
			device: device_id.clone(),
			position,
			ticks: ticks.into(),
		},
	};
	let touchscreen_press = |position, x, y, hold| inbound::PayloadEvent {
		payload: inbound::devices::TouchscreenPressPayload {
			device: device_id.clone(),
			position,
			x,
			y,
			hold,
		},
	};
	loop {
		let updates = match reader.read(100.0).await {
			Ok(updates) => updates,
			Err(_) => break,
		};
		for update in updates {
			match match update {
				DeviceStateUpdate::ButtonDown(key) => inbound::devices::key_down(press(key)).await,
				DeviceStateUpdate::ButtonUp(key) => inbound::devices::key_up(press(key)).await,
				DeviceStateUpdate::TouchPointDown(point) => inbound::devices::key_down(press(kind.key_count() + point)).await,
				DeviceStateUpdate::TouchPointUp(point) => inbound::devices::key_up(press(kind.key_count() + point)).await,
				DeviceStateUpdate::EncoderTwist(dial, ticks) => inbound::devices::encoder_change(encoder(dial, ticks)).await,
				DeviceStateUpdate::EncoderDown(dial) => inbound::devices::encoder_down(press(dial)).await,
				DeviceStateUpdate::EncoderUp(dial) => inbound::devices::encoder_up(press(dial)).await,
				DeviceStateUpdate::TouchScreenPress(x, y) => {
					let (position, x, y) = match kind {
						Kind::Plus | Kind::PlusXl => ((x / 200) as u8, x % 200, y),
						_ => continue,
					};
					inbound::devices::touchscreen_press(touchscreen_press(position, x, y, false)).await
				}
				DeviceStateUpdate::TouchScreenLongPress(x, y) => {
					let (position, x, y) = match kind {
						Kind::Plus | Kind::PlusXl => ((x / 200) as u8, x % 200, y),
						_ => continue,
					};
					inbound::devices::touchscreen_press(touchscreen_press(position, x, y, true)).await
				}
				_ => Ok(()),
			} {
				Ok(_) => (),
				Err(error) => log::warn!("Failed to process device event {update:?}: {error}"),
			}
		}
	}

	ELGATO_DEVICES.write().await.remove(&device_id);
	crate::events::inbound::devices::deregister_device("", crate::events::inbound::PayloadEvent { payload: device_id })
		.await
		.unwrap();
}

/// Attempt to initialise all connected devices.
pub async fn initialise_devices() {
	if crate::store::get_settings().value.disableelgato {
		crate::plugins::DEVICE_NAMESPACES
			.write()
			.await
			.insert("sd".to_owned(), "opendeck_alternative_elgato_implementation".to_owned());
		return;
	} else {
		crate::plugins::DEVICE_NAMESPACES.write().await.remove("sd");
	}

	// Iterate through detected Elgato devices and attempt to register them.
	let current = HIDAPI.read().await.as_ref().cloned();
	let hid = match current {
		Some(arc) => arc,
		None => match elgato_streamdeck::new_hidapi() {
			Ok(hid) => {
				let arc = Arc::new(hid);
				HIDAPI.write().await.replace(arc.clone());
				arc
			}
			Err(error) => {
				log::warn!("Failed to initialise hidapi: {error}");
				return;
			}
		},
	};
	for (kind, serial) in elgato_streamdeck::asynchronous::list_devices_async(&hid) {
		let device_id = format!("sd-{serial}");
		if ELGATO_DEVICES.read().await.contains_key(&device_id) {
			continue;
		}
		match elgato_streamdeck::AsyncStreamDeck::connect(&hid, kind, &serial) {
			Ok(device) => {
				tokio::spawn(init(device, device_id));
			}
			Err(error) => log::warn!("Failed to connect to Elgato device: {error}"),
		}
	}
}
