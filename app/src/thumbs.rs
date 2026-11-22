//! Decoding of stored thumbnails. The bytes come from the sender, so a
//! thumbnail may declare a huge canvas to exhaust memory; the declared size
//! is checked before anything is allocated.

use std::collections::HashSet;
use std::io::Cursor;
use std::sync::Mutex;

use image::{ImageReader, Limits};
use slint::{Rgba8Pixel, SharedPixelBuffer};

/// Largest declared width / height accepted.
const MAX_DECLARED: u32 = 512;
/// Decoder allocation cap.
const MAX_ALLOC: u64 = 4 * 1024 * 1024;
/// Longest edge of the decoded result.
const THUMB_DIM: u32 = 160;
/// Failed ids remembered before the set is started over.
const FAILED_CAP: usize = 4096;

static FAILED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

pub fn decode_thumbnail(bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if w == 0 || h == 0 || w > MAX_DECLARED || h > MAX_DECLARED {
        return None;
    }
    let mut reader = ImageReader::new(Cursor::new(bytes)).with_guessed_format().ok()?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_DECLARED);
    limits.max_image_height = Some(MAX_DECLARED);
    limits.max_alloc = Some(MAX_ALLOC);
    reader.limits(limits);
    let img = reader.decode().ok()?;
    let img = if img.width() > THUMB_DIM || img.height() > THUMB_DIM {
        img.thumbnail(THUMB_DIM, THUMB_DIM)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    Some(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(rgba.as_raw(), w, h))
}

/// `decode_thumbnail` that remembers failures by `key`, so a broken thumbnail
/// is not decoded again on every refresh.
pub fn decode_thumbnail_cached(key: &str, bytes: &[u8]) -> Option<SharedPixelBuffer<Rgba8Pixel>> {
    let known_bad = {
        let guard = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        guard.as_ref().is_some_and(|s| s.contains(key))
    };
    if known_bad {
        return None;
    }
    let decoded = decode_thumbnail(bytes);
    if decoded.is_none() {
        let mut guard = FAILED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let set = guard.get_or_insert_with(HashSet::new);
        if set.len() >= FAILED_CAP {
            set.clear();
        }
        set.insert(key.to_string());
    }
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageFormat, RgbImage};

    fn encode(w: u32, h: u32, fmt: ImageFormat) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| image::Rgb([(x % 256) as u8, (y % 256) as u8, 90]));
        let mut out = Vec::new();
        img.write_to(&mut Cursor::new(&mut out), fmt).expect("encode");
        out
    }

    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for b in data {
            crc ^= u32::from(*b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn decodes_jpeg_and_png() {
        let jpeg = decode_thumbnail(&encode(96, 64, ImageFormat::Jpeg)).expect("jpeg");
        assert_eq!((jpeg.width(), jpeg.height()), (96, 64));
        let png = decode_thumbnail(&encode(32, 32, ImageFormat::Png)).expect("png");
        assert_eq!((png.width(), png.height()), (32, 32));
    }

    #[test]
    fn large_images_are_downscaled() {
        let buf = decode_thumbnail(&encode(400, 200, ImageFormat::Png)).expect("png");
        assert!(buf.width() <= THUMB_DIM && buf.height() <= THUMB_DIM);
        assert_eq!(buf.width(), THUMB_DIM);
    }

    #[test]
    fn declared_huge_canvas_is_rejected() {
        let mut png = encode(8, 8, ImageFormat::Png);
        // IHDR data starts at byte 16: width, height; its CRC follows at 29.
        png[16..20].copy_from_slice(&20_000u32.to_be_bytes());
        png[20..24].copy_from_slice(&20_000u32.to_be_bytes());
        let crc = crc32(&png[12..29]);
        png[29..33].copy_from_slice(&crc.to_be_bytes());
        assert!(decode_thumbnail(&png).is_none());
    }

    #[test]
    fn garbage_is_rejected() {
        assert!(decode_thumbnail(b"definitely not an image").is_none());
        assert!(decode_thumbnail(&[]).is_none());
    }

    #[test]
    fn failures_are_remembered() {
        assert!(decode_thumbnail_cached("neg-1", b"junk").is_none());
        let known = FAILED.lock().expect("lock").as_ref().is_some_and(|s| s.contains("neg-1"));
        assert!(known);
        // Valid bytes under a remembered key are not decoded again.
        assert!(decode_thumbnail_cached("neg-1", &encode(8, 8, ImageFormat::Png)).is_none());
    }
}
