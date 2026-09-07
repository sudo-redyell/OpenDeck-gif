//! Animated GIF support for Stream Deck buttons.
//!
//! Stream Deck hardware has no concept of animation; images are written to
//! buttons as single frames. This module decomposes an animated GIF into a list
//! of composited frames with per-frame delays so that the device can be driven
//! frame-by-frame (see `elgato.rs`).
//!
//! Frame composition mirrors the frontend renderer (`src/lib/rendererHelper.ts`
//! `renderImage`) so animated buttons match the static rendering path: a
//! logical 144x144 canvas, a background colour fill (unless the colour starts
//! with "#000000"), and the state's image scale applied around the centre.

use std::io::Cursor;
use std::time::Duration;

use base64::Engine as _;
use image::codecs::gif::GifDecoder;
use image::imageops::{self, FilterType};
use image::{AnimationDecoder, Delay, Rgba, RgbaImage};

/// Logical canvas size; mirrors the frontend renderer's 144x144 canvas.
const CANVAS_SIZE: u32 = 144;
/// Maximum number of frames accepted from a single GIF.
const MAX_FRAMES: usize = 600;
/// Maximum decoded frame dimension, in pixels.
const MAX_FRAME_DIM: u32 = 4096;
/// Floor for per-frame delays; protects the HID write pipeline and keeps the
/// loop ticking at a rate the device can keep up with.
const MIN_FRAME_DELAY_MS: u32 = 20;

/// A decomposed animated image: composited frames paired with their delays.
#[derive(Debug, Clone)]
pub struct AnimatedImage {
	pub frames: Vec<RgbaImage>,
	pub delays: Vec<Duration>,
}

impl AnimatedImage {
	#[cfg(test)]
	pub fn frame_count(&self) -> usize {
		self.frames.len()
	}
}

/// Returns whether the bytes look like a GIF image.
pub fn is_gif(bytes: &[u8]) -> bool {
	bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")
}

/// Decodes an animated GIF into composited frames and delays.
///
/// Returns `Ok(None)` when the bytes are not a GIF or contain fewer than two
/// frames (a static GIF follows the regular static image path).
pub fn decode_animated_gif(bytes: &[u8]) -> Result<Option<AnimatedImage>, String> {
	if !is_gif(bytes) {
		return Ok(None);
	}

	let decoder = GifDecoder::new(Cursor::new(bytes)).map_err(|error| format!("GIF decoder error: {error}"))?;

	let mut frames: Vec<RgbaImage> = Vec::new();
	let mut delays: Vec<Duration> = Vec::new();
	for frame in decoder.into_frames() {
		let frame = frame.map_err(|error| format!("GIF frame error: {error}"))?;
		if frames.len() >= MAX_FRAMES {
			return Err(format!("GIF exceeds the maximum frame count ({MAX_FRAMES})"));
		}

		let (width, height) = (frame.buffer().width(), frame.buffer().height());
		if width > MAX_FRAME_DIM || height > MAX_FRAME_DIM {
			return Err(format!("GIF frame of {width}x{height}px exceeds the maximum frame dimension ({MAX_FRAME_DIM}px)"));
		}

		frames.push(frame.buffer().clone());
		delays.push(clamp_delay(frame.delay()));
	}

	if frames.len() < 2 {
		return Ok(None);
	}
	Ok(Some(AnimatedImage { frames, delays }))
}

/// Reads the frame duration, enforcing the minimum frame delay.
fn clamp_delay(delay: Delay) -> Duration {
	// Round instead of integer-dividing: 33ms written as 33/100 would collapse to 0.
	let (numer, denom) = delay.numer_denom_ms();
	let ms = if denom == 0 { numer } else { (f64::from(numer) / f64::from(denom)).round() as u32 };
	Duration::from_millis(u64::from(ms.max(MIN_FRAME_DELAY_MS)))
}

/// Extracts the base64 payload of a `data:` URL.
///
/// Returns `None` for anything that is not a base64 data URL, so callers can
/// fall back to their usual handling instead of panicking on odd input.
pub fn extract_base64_payload(image: &str) -> Option<Vec<u8>> {
	let meta = image.strip_prefix("data:")?;
	let (meta, payload) = meta.split_once(',')?;
	if !meta.rsplit(';').any(|part| part.eq_ignore_ascii_case("base64")) {
		return None;
	}
	base64::engine::general_purpose::STANDARD.decode(payload.trim()).ok()
}

/// Parses a hex colour (`#rgb`, `#rrggbb` or `#rrggbbaa`, case-insensitive).
fn parse_hex_colour(colour: &str) -> Option<(u8, u8, u8, u8)> {
	let hex = colour.strip_prefix('#')?;
	if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
		return None;
	}

	let expand = |value: &str| u8::from_str_radix(value, 16).ok().map(|v| v * 17);
	match hex.len() {
		3 => {
			let r = expand(&hex[0..1])?;
			let g = expand(&hex[1..2])?;
			let b = expand(&hex[2..3])?;
			Some((r, g, b, 255))
		}
		6 => Some((
			u8::from_str_radix(&hex[0..2], 16).ok()?,
			u8::from_str_radix(&hex[2..4], 16).ok()?,
			u8::from_str_radix(&hex[4..6], 16).ok()?,
			255,
		)),
		8 => Some((
			u8::from_str_radix(&hex[0..2], 16).ok()?,
			u8::from_str_radix(&hex[2..4], 16).ok()?,
			u8::from_str_radix(&hex[4..6], 16).ok()?,
			u8::from_str_radix(&hex[6..8], 16).ok()?,
		)),
		_ => None,
	}
}

/// Composites a frame onto the logical canvas, mirroring the frontend renderer.
///
/// - The canvas is [`CANVAS_SIZE`] logical pixels per side.
/// - The background colour is filled first, unless it starts with "#000000"
///   (matching the frontend's skip for black backgrounds).
/// - The frame is stretched to `canvas * image_scale / 100` around the centre;
///   scales above 100 are clipped by the canvas.
pub fn compose_frame(frame: &RgbaImage, background: Option<&str>, image_scale: Option<u8>) -> RgbaImage {
	let mut canvas = RgbaImage::new(CANVAS_SIZE, CANVAS_SIZE);

	if let Some(colour) = background.filter(|background| !background.starts_with("#000000"))
		&& let Some((r, g, b, a)) = parse_hex_colour(colour)
	{
		canvas.pixels_mut().for_each(|pixel| *pixel = Rgba([r, g, b, a]));
	}

	// The frontend stretches the image to canvas*scale/100 and clips overflow;
	// sizes above the canvas are kept and centered at negative offsets.
	let scale = (u32::from(image_scale.unwrap_or(100)) as u64).max(10);
	let size = (u64::from(CANVAS_SIZE) * scale / 100).max(1) as u32;
	let scaled = imageops::resize(frame, size, size, FilterType::Lanczos3);
	let offset = i64::from(CANVAS_SIZE) / 2 - i64::from(size) / 2;
	imageops::overlay(&mut canvas, &scaled, offset, offset);
	canvas
}

#[cfg(test)]
mod tests {
	use super::*;

	use std::path::{Path, PathBuf};

	fn fixture(name: &str) -> PathBuf {
		Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(name)
	}

	fn read_fixture(name: &str) -> Vec<u8> {
		std::fs::read(fixture(name)).expect("fixture should exist")
	}

	#[test]
	fn decodes_two_frame_swap() {
		let bytes = read_fixture("swap.gif");
		let animation = decode_animated_gif(&bytes).unwrap().expect("swap.gif is animated");

		assert_eq!(animation.frame_count(), 2);
		assert_eq!(animation.delays[0], Duration::from_millis(50));
		assert_eq!(animation.delays[1], Duration::from_millis(100));

		assert_eq!(animation.frames[0].get_pixel(0, 0).0, [255, 0, 0, 255]);
		assert_eq!(animation.frames[1].get_pixel(0, 0).0, [0, 0, 255, 255]);
	}

	#[test]
	fn treats_static_gif_as_not_animated() {
		let bytes = read_fixture("static.gif");
		assert!(decode_animated_gif(&bytes).unwrap().is_none());
	}

	#[test]
	fn rejects_non_gif_bytes() {
		assert!(decode_animated_gif(b"\x89PNG\r\n\x1a\nnot a gif").unwrap().is_none());
	}

	/// disposal=1: frame 2 only carries a moved rectangle, so decoding must
	/// composite it onto the previous frame.
	#[test]
	fn composites_disposal1_frames() {
		let bytes = read_fixture("disposal1.gif");
		let animation = decode_animated_gif(&bytes).unwrap().expect("disposal1.gif is animated");

		assert_eq!(animation.frame_count(), 2);
		// The moved rectangle is solid in frame 2.
		assert_eq!(animation.frames[1].get_pixel(48, 48).0, [0, 0, 255, 255]);
		// Far from the rectangle, frame 2 must show the first frame's content.
		assert_eq!(animation.frames[1].get_pixel(0, 0).0, [255, 0, 0, 255]);
	}

	/// disposal=2: the previous rectangle is restored to the background in
	/// frame 2 instead of remaining visible.
	#[test]
	fn composites_disposal2_frames() {
		let bytes = read_fixture("disposal2.gif");
		let animation = decode_animated_gif(&bytes).unwrap().expect("disposal2.gif is animated");

		assert_eq!(animation.frame_count(), 2);
		// Restored area (frame 1's rectangle) must no longer show blue...
		assert_eq!(animation.frames[1].get_pixel(8, 8).0[0], 255);
		assert_ne!(animation.frames[1].get_pixel(8, 8).0, [0, 0, 255, 255]);
		// ...and the moved rectangle must be present at its new position.
		assert_eq!(animation.frames[1].get_pixel(48, 48).0, [0, 0, 255, 255]);
	}

	#[test]
	fn clamps_tiny_frame_delays() {
		let bytes = read_fixture("tiny_delay.gif");
		let animation = decode_animated_gif(&bytes).unwrap().expect("tiny_delay.gif is animated");
		for delay in animation.delays {
			assert!(delay >= Duration::from_millis(u64::from(MIN_FRAME_DELAY_MS)));
		}
	}

	#[test]
	fn extracts_base64_payload() {
		let data = b"gif-bytes";
		let encoded = base64::engine::general_purpose::STANDARD.encode(data);
		let url = format!("data:image/gif;base64,{encoded}");
		assert_eq!(extract_base64_payload(&url).as_deref(), Some(data.as_slice()));

		let urlencoded = "data:image/gif,%3Csvg%3E";
		assert!(extract_base64_payload(urlencoded).is_none());
		assert!(extract_base64_payload("/path/without/comma").is_none());
	}

	#[test]
	fn compose_frame_matches_frontend_canvas() {
		let bytes = read_fixture("swap.gif");
		let animation = decode_animated_gif(&bytes).unwrap().unwrap();

		// No background: transparent corners with the frame centred.
		let composed = compose_frame(&animation.frames[0], None, Some(50));
		assert_eq!(composed.get_pixel(0, 0).0[3], 0);
		assert_eq!(composed.get_pixel(CANVAS_SIZE / 2, CANVAS_SIZE / 2).0, [255, 0, 0, 255]);

		// Background colour shows through wherever the frame leaves canvas space.
		let composed = compose_frame(&animation.frames[0], Some("#00FF00"), Some(50));
		assert_eq!(composed.get_pixel(0, 0).0, [0, 255, 0, 255]);
		assert_eq!(composed.get_pixel(CANVAS_SIZE / 2, CANVAS_SIZE / 2).0, [255, 0, 0, 255]);

		// Black backgrounds are skipped, leaving transparency like the frontend.
		let composed = compose_frame(&animation.frames[0], None, Some(50));
		assert_eq!(composed.get_pixel(0, 0).0[3], 0);

		// Scaling above 100 clips to the canvas.
		let composed = compose_frame(&animation.frames[0], None, Some(200));
		assert_eq!(composed.get_pixel(0, 0).0, [255, 0, 0, 255]);
	}
}
