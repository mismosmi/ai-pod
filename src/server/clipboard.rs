//! Host-side clipboard reader for the container paste bridge.
//!
//! Claude Code, running inside an ai-pod container, reads the clipboard by
//! shelling out to `xclip`/`wl-paste`. Those tools (and any X11/Wayland socket)
//! are absent in the container, so paste fails. The container instead forwards
//! the read to the host's `GET /clipboard/image` endpoint, which calls
//! [`read_clipboard_png`] here on the host where the real clipboard lives.
//!
//! Only image bytes are ever returned — never clipboard text — which keeps the
//! bridge from leaking arbitrary copied secrets into the container.

/// PNG file signature (the first 8 bytes of every PNG).
const PNG_MAGIC: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Return `bytes` only if they begin with the PNG magic. Anything else (empty
/// output, an error string printed to stdout, a truncated read) yields `None`,
/// so the endpoint never reports non-image data as an image.
fn valid_png(bytes: Vec<u8>) -> Option<Vec<u8>> {
    if bytes.starts_with(PNG_MAGIC) {
        Some(bytes)
    } else {
        None
    }
}

/// Read the host clipboard and return it as PNG bytes, or `None` when the
/// clipboard holds no image (or the platform has no supported reader).
///
/// Blocking: shells out to platform tools. Callers on the async server run this
/// inside `spawn_blocking`.
#[cfg(target_os = "macos")]
pub fn read_clipboard_png() -> Option<Vec<u8>> {
    use std::process::Command;

    // Fast path: pngpaste streams PNG bytes straight to stdout when present.
    if let Ok(out) = Command::new("pngpaste").arg("-").output() {
        if out.status.success() {
            if let Some(png) = valid_png(out.stdout) {
                return Some(png);
            }
        }
    }

    // Fallback: osascript coerces the clipboard to PNG and writes it to a temp
    // file (AppleScript can't reliably emit raw bytes on stdout). If the
    // clipboard has no image, the `as «class PNGf»` coercion errors and the
    // script aborts before writing — leaving us with `None`.
    let path = std::env::temp_dir().join(format!(
        "ai-pod-clip-{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let script = format!(
        "set thePath to \"{}\"\n\
         set theData to (the clipboard as «class PNGf»)\n\
         set theFile to open for access POSIX file thePath with write permission\n\
         set eof of theFile to 0\n\
         write theData to theFile\n\
         close access theFile",
        path.display()
    );
    let status = Command::new("osascript").arg("-e").arg(&script).output();
    let result = match status {
        Ok(out) if out.status.success() => std::fs::read(&path).ok().and_then(valid_png),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    result
}

/// Read the host clipboard and return it as PNG bytes, or `None` when the
/// clipboard holds no image (or the platform has no supported reader).
#[cfg(target_os = "linux")]
pub fn read_clipboard_png() -> Option<Vec<u8>> {
    use std::process::Command;

    // Wayland first (host OS priority), then X11 as a free fallback.
    let attempts: [(&str, &[&str]); 2] = [
        ("wl-paste", &["--type", "image/png"]),
        (
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        ),
    ];
    for (bin, args) in attempts {
        if let Ok(out) = Command::new(bin).args(args).output() {
            if out.status.success() {
                if let Some(png) = valid_png(out.stdout) {
                    return Some(png);
                }
            }
        }
    }
    None
}

/// Platforms without a supported clipboard reader: paste simply yields nothing.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read_clipboard_png() -> Option<Vec<u8>> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_png_accepts_real_magic() {
        let mut bytes = PNG_MAGIC.to_vec();
        bytes.extend_from_slice(b"...rest of a png...");
        assert_eq!(valid_png(bytes.clone()), Some(bytes));
    }

    #[test]
    fn valid_png_rejects_empty() {
        assert_eq!(valid_png(Vec::new()), None);
    }

    #[test]
    fn valid_png_rejects_text() {
        assert_eq!(valid_png(b"xclip: error: no image".to_vec()), None);
    }

    #[test]
    fn valid_png_rejects_truncated_magic() {
        // First few bytes of the magic but cut short — not a PNG.
        assert_eq!(valid_png(b"\x89PNG".to_vec()), None);
    }
}
