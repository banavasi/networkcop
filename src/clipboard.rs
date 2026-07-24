//! Copy to the system clipboard.
//!
//! No Rust dependency: shells out to whatever the platform provides, and falls
//! back to an OSC 52 escape sequence, which is what works over SSH and inside
//! tmux where no clipboard helper exists.

use anyhow::{bail, Result};
use std::io::Write;
use std::process::{Command, Stdio};

/// Helpers in preference order. Wayland first — an X11 helper on a Wayland
/// session writes to XWayland's clipboard, which native apps never see.
const HELPERS: &[(&str, &[&str])] = &[
    ("wl-copy", &[]),
    ("xclip", &["-selection", "clipboard"]),
    ("xsel", &["--clipboard", "--input"]),
    ("pbcopy", &[]),
];

/// Copy `text`, returning which mechanism took it, for the status line.
pub fn copy(text: &str) -> Result<&'static str> {
    for (bin, args) in HELPERS {
        if !on_path(bin) {
            continue;
        }
        if pipe_to(bin, args, text).is_ok() {
            return Ok(*bin);
        }
    }
    osc52(text)?;
    Ok("terminal")
}

fn on_path(bin: &str) -> bool {
    std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
        .unwrap_or(false)
}

fn pipe_to(bin: &str, args: &[&str], text: &str) -> Result<()> {
    let mut child = Command::new(bin)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?
        .write_all(text.as_bytes())?;
    // wl-copy forks a server to own the selection; don't block on it
    if bin == "wl-copy" {
        return Ok(());
    }
    if !child.wait()?.success() {
        bail!("{bin} exited non-zero");
    }
    Ok(())
}

/// OSC 52. Terminals cap the payload; 100 kB is past every limit worth trying.
fn osc52(text: &str) -> Result<()> {
    const LIMIT: usize = 100_000;
    if text.len() > LIMIT {
        bail!("no clipboard helper found, and {} bytes is too large for the terminal escape (install wl-copy or xclip)", text.len());
    }
    let payload = base64(text.as_bytes());
    let seq = format!("\x1b]52;c;{payload}\x07");
    let mut out = std::io::stdout();
    out.write_all(seq.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// Small standalone base64 — not worth a dependency for one escape sequence.
fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// "copied 1.2 kB via wl-copy" — what the status line shows.
pub fn describe(bytes: usize, via: &str) -> String {
    format!("copied {} via {via}", crate::app::human_size(bytes as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(base64(b"fooba"), "Zm9vYmE=");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn base64_handles_binary_and_high_bytes() {
        assert_eq!(base64(&[0xff, 0xfe, 0xfd]), "//79");
        assert_eq!(base64(&[0x00, 0x00, 0x00]), "AAAA");
        // length is always a multiple of 4
        for n in 0..32 {
            assert_eq!(base64(&vec![b'x'; n]).len() % 4, 0, "n={n}");
        }
    }

    #[test]
    fn oversized_payload_is_refused_with_actionable_advice() {
        let huge = "x".repeat(200_000);
        let err = osc52(&huge).unwrap_err().to_string();
        assert!(err.contains("too large"), "{err}");
        assert!(err.contains("wl-copy"), "should name a fix: {err}");
    }

    #[test]
    fn describe_reads_naturally() {
        assert_eq!(describe(1536, "wl-copy"), "copied 1.5 kB via wl-copy");
    }
}
