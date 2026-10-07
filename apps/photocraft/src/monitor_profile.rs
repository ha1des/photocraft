//! The main display's ICC profile, for the colour-managed canvas (Edit › Color Settings ›
//! Monitor Profile = `auto`).
//!
//! macOS: `NSScreen.mainScreen.colorSpace.ICCProfileData`, a documented AppKit API, read through
//! `osascript` (AppKit via AppleScriptObjC) so the app needs no `unsafe` FFI. The query runs on a
//! background thread at launch (about 0.4 s) and the profile is applied when it arrives, however
//! late. The helper process has no windows, so "main screen" is the menu-bar display, not
//! necessarily the one showing the canvas (#569): the result names the display it read, and
//! Help › System Info shows it. Other platforms report no profile (sRGB, or the profile chosen
//! in Color Settings).

use std::sync::mpsc::Receiver;

use photocraft_engine::display_color::DetectedMonitor;

/// What the platform reader returns: the profile, or why there is none.
pub type Detection = Result<DetectedMonitor, String>;

/// Starts reading the main display's profile in the background; `None` when the platform has
/// no reader.
pub fn detect_async() -> Option<Receiver<Detection>> {
    if !cfg!(target_os = "macos") {
        return None;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(detect());
    });
    Some(rx)
}

#[cfg(target_os = "macos")]
fn detect() -> Detection {
    let out = std::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            "use framework \"AppKit\"",
            "-e",
            "set s to current application's NSScreen's mainScreen()",
            "-e",
            "return ((s's localizedName()) as text) & linefeed & ((s's colorSpace()'s ICCProfileData()'s base64EncodedStringWithOptions:0) as text)",
        ])
        .output()
        .map_err(|e| format!("couldn't run osascript to read the display profile: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let first = err.lines().next().unwrap_or("").trim();
        return Err(format!("reading the display profile failed ({}){}", out.status, if first.is_empty() { String::new() } else { format!(": {first}") }));
    }
    parse_reply(&String::from_utf8_lossy(&out.stdout))
}

#[cfg(not(target_os = "macos"))]
fn detect() -> Detection {
    Err("this platform doesn't report display profiles".into())
}

/// The helper's reply: the display name, a newline, then the profile as base64.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn parse_reply(out: &str) -> Detection {
    let (display, b64) = out.split_once('\n').unwrap_or((out, ""));
    let display = display.trim();
    let display = if display.is_empty() { "the main display" } else { display };
    let b64 = b64.trim();
    if b64.is_empty() {
        return Err(format!("{display} has no ICC profile"));
    }
    let icc = base64_decode(b64).ok_or_else(|| format!("{display}'s profile data couldn't be decoded"))?;
    // An ICC profile starts with its size and carries `acsp` at offset 36.
    if !(icc.len() >= 132 && icc.get(36..40) == Some(b"acsp")) {
        return Err(format!("{display}'s profile data isn't an ICC profile"));
    }
    Ok(DetectedMonitor { display: display.to_string(), icc })
}

/// Standard base64 (RFC 4648, with padding) → bytes; `None` on any invalid character.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        } as u32)
    };
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        let mut acc = 0u32;
        for (i, c) in chunk.iter().enumerate() {
            acc |= val(*c)? << (18 - 6 * i);
        }
        let n = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => return None,
        };
        out.extend_from_slice(&acc.to_be_bytes()[1..1 + n]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64() {
        assert_eq!(base64_decode("aGVsbG8=").as_deref(), Some(&b"hello"[..]));
        assert_eq!(base64_decode("aGVsbG8h").as_deref(), Some(&b"hello!"[..]));
        assert_eq!(base64_decode("aGk=").as_deref(), Some(&b"hi"[..]));
        assert_eq!(base64_decode("").as_deref(), Some(&b""[..]));
        assert!(base64_decode("a").is_none());
        assert!(base64_decode("a$==").is_none());
    }

    #[test]
    fn helper_replies() {
        // Only the ICC signature is checked here; parsing is the engine's job.
        let mut icc = vec![0u8; 132];
        icc[36..40].copy_from_slice(b"acsp");
        let b64 = base64_encode(&icc);
        let m = parse_reply(&format!("ROG PG32UQX\n{b64}\n")).unwrap();
        assert_eq!((m.display.as_str(), &m.icc), ("ROG PG32UQX", &icc));
        assert_eq!(parse_reply(&format!("\n{b64}")).unwrap().display, "the main display");
        assert!(parse_reply("Built-in Retina Display\n").unwrap_err().contains("Built-in Retina Display has no ICC profile"));
        assert!(parse_reply("").is_err());
        assert!(parse_reply("X\n%%%").unwrap_err().contains("couldn't be decoded"));
        assert!(parse_reply("X\naGVsbG8=").unwrap_err().contains("isn't an ICC profile"));
    }

    fn base64_encode(b: &[u8]) -> String {
        const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut s = String::new();
        for c in b.chunks(3) {
            let n = c.iter().enumerate().fold(0u32, |n, (i, v)| n | u32::from(*v) << (16 - 8 * i));
            for i in 0..=c.len() {
                s.push(A[(n >> (18 - 6 * i) & 63) as usize] as char);
            }
        }
        while !s.len().is_multiple_of(4) {
            s.push('=');
        }
        s
    }

    /// The detected profile (when there is one) parses as an RGB profile.
    #[test]
    fn detected_profile_parses() {
        if let Ok(m) = detect() {
            let p = photocraft_engine::color_cmds::profile_from_bytes(&std::sync::Arc::new(m.icc)).expect("parses");
            assert_eq!(format!("{:?}", p.color_space), "Rgb", "{}", p.description);
        }
    }
}
