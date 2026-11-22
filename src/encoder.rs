//! Off-thread PNG encoder and thumbnail generator.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct EncodedCapture {
    pub path: PathBuf,
    pub png: Arc<[u8]>,
    pub thumb_width: u32,
    pub thumb_height: u32,
    pub thumb_premultiplied: Vec<u8>,
}

pub fn encode_and_save(
    directory: &Path,
    explicit_path: Option<&Path>,
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<EncodedCapture, Box<dyn std::error::Error + Send + Sync>> {
    let png_bytes = encode_png_bytes(width, height, rgba)?;
    let path = match explicit_path {
        Some(target) => {
            if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
                fs::create_dir_all(parent)?;
            }
            fs::write(target, &png_bytes)?;
            target.to_path_buf()
        }
        None => {
            fs::create_dir_all(directory)?;
            let (path, mut file) = create_unique_file(directory)?;
            if let Err(err) = io::Write::write_all(&mut BufWriter::new(&mut file), &png_bytes) {
                let _ = fs::remove_file(&path);
                return Err(err.into());
            }
            path
        }
    };

    let (thumb_width, thumb_height, thumb_premultiplied) = make_thumbnail(width, height, rgba);
    Ok(EncodedCapture {
        path,
        png: Arc::from(png_bytes),
        thumb_width,
        thumb_height,
        thumb_premultiplied,
    })
}

pub fn encode_png_bytes(
    width: u32,
    height: u32,
    rgba: &[u8],
) -> Result<Vec<u8>, png::EncodingError> {
    let mut buf = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut buf, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header()?;
        writer.write_image_data(rgba)?;
    }
    Ok(buf)
}

fn create_unique_file(directory: &Path) -> io::Result<(PathBuf, File)> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    for suffix in 0..1000u16 {
        let path = directory.join(format!(
            "swm-screenshot-{}-{:03}-{suffix}.png",
            now.as_secs(),
            now.subsec_millis()
        ));
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not allocate a unique screenshot filename",
    ))
}

fn make_thumbnail(width: u32, height: u32, rgba: &[u8]) -> (u32, u32, Vec<u8>) {
    if width == 0 || height == 0 {
        return (1, 1, vec![0, 0, 0, 0]);
    }
    let max_w = 328u32;
    let max_h = 164u32;
    let scale = (max_w as f64 / width as f64)
        .min(max_h as f64 / height as f64)
        .min(1.0);
    let tw = ((width as f64 * scale).round() as u32).max(1);
    let th = ((height as f64 * scale).round() as u32).max(1);
    let mut out = vec![0u8; (tw * th * 4) as usize];
    for y in 0..th {
        let sy = ((y as f64 + 0.5) * height as f64 / th as f64).floor() as u32;
        let sy = sy.min(height - 1);
        for x in 0..tw {
            let sx = ((x as f64 + 0.5) * width as f64 / tw as f64).floor() as u32;
            let sx = sx.min(width - 1);
            let s_idx = ((sy * width + sx) * 4) as usize;
            let d_idx = ((y * tw + x) * 4) as usize;
            let r = u16::from(rgba[s_idx]);
            let g = u16::from(rgba[s_idx + 1]);
            let b = u16::from(rgba[s_idx + 2]);
            let a = u16::from(rgba[s_idx + 3]);
            // tiny-skia wants premul RGBA; R/B swap happens at shm copy.
            out[d_idx] = ((r * a) / 255) as u8;
            out[d_idx + 1] = ((g * a) / 255) as u8;
            out[d_idx + 2] = ((b * a) / 255) as u8;
            out[d_idx + 3] = a as u8;
        }
    }
    (tw, th, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_valid_png_and_creates_file() {
        let dir = std::env::temp_dir().join(format!("swm-shot-enc-{}", std::process::id()));
        let rgba = [12, 34, 56, 255].repeat(4 * 4);
        let encoded = encode_and_save(&dir, None, 4, 4, &rgba).unwrap();
        assert!(encoded.path.exists());
        assert_eq!(&encoded.png[0..8], b"\x89PNG\r\n\x1a\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
