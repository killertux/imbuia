//! Client-side system clipboard ingestion for remote-session paste.

use anyhow::{Context, Result, anyhow, bail};
use image::{DynamicImage, ImageFormat, RgbaImage};
use std::io::Cursor;

pub const MAX_UPLOAD_BYTES: usize = crate::ipc::MAX_UPLOAD_BYTES as usize;

pub enum ClipboardContent {
    Text(String),
    File { name: String, bytes: Vec<u8> },
}

impl std::fmt::Debug for ClipboardContent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Text(text) => f.debug_tuple("Text").field(&text.len()).finish(),
            Self::File { name, bytes } => f
                .debug_struct("File")
                .field("name", name)
                .field("bytes", &bytes.len())
                .finish(),
        }
    }
}

/// Read one useful item from the workstation clipboard. This is blocking and
/// must be called off the TUI/runtime task.
pub fn read() -> Result<ClipboardContent> {
    let mut cb = arboard::Clipboard::new().context("open system clipboard")?;

    if let Ok(files) = cb.get().file_list()
        && !files.is_empty()
    {
        if files.len() != 1 {
            bail!("copy one file at a time (found {})", files.len());
        }
        let path = &files[0];
        let meta = std::fs::metadata(path).context("inspect clipboard file")?;
        if !meta.is_file() {
            bail!("clipboard item is not a regular file");
        }
        let size: usize = meta
            .len()
            .try_into()
            .map_err(|_| anyhow!("clipboard file is too large"))?;
        enforce_size(size)?;
        let bytes = std::fs::read(path).context("read clipboard file")?;
        enforce_size(bytes.len())?;
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("clipboard-file")
            .to_string();
        return Ok(ClipboardContent::File { name, bytes });
    }

    if let Ok(img) = cb.get_image() {
        let width: u32 = img.width.try_into().context("clipboard image width")?;
        let height: u32 = img.height.try_into().context("clipboard image height")?;
        let rgba = RgbaImage::from_raw(width, height, img.bytes.into_owned())
            .ok_or_else(|| anyhow!("invalid clipboard image buffer"))?;
        let mut bytes = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .context("encode clipboard image as PNG")?;
        enforce_size(bytes.len())?;
        return Ok(ClipboardContent::File {
            name: "clipboard.png".into(),
            bytes,
        });
    }

    let text = cb
        .get_text()
        .context("clipboard has no text, file, or image")?;
    Ok(ClipboardContent::Text(text))
}

fn enforce_size(size: usize) -> Result<()> {
    if size > MAX_UPLOAD_BYTES {
        bail!(
            "clipboard file is {:.1} MiB; maximum is {} MiB",
            size as f64 / (1024.0 * 1024.0),
            MAX_UPLOAD_BYTES / (1024 * 1024)
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_limit_accepts_boundary() {
        assert!(enforce_size(MAX_UPLOAD_BYTES).is_ok());
        assert!(enforce_size(MAX_UPLOAD_BYTES + 1).is_err());
    }
}
