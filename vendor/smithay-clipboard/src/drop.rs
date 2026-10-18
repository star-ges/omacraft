//! Files dropped on the app's windows (added by Omacraft).
//!
//! winit has no drag and drop on Wayland, and compositors like Hyprland send drops only to a
//! client's first data device, which is this crate's. So the clipboard's data device takes
//! `text/uri-list` drops too and queues the paths here for the app to take.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};

pub(crate) const URI_LIST: &str = "text/uri-list";

static DROPPED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
static HOVERING: AtomicBool = AtomicBool::new(false);
static NOTIFY: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

/// Paths dropped since the last call, oldest first.
pub fn take_dropped() -> Vec<PathBuf> {
    std::mem::take(&mut *DROPPED.lock().unwrap())
}

/// Whether a drag of files is over one of the app's windows.
pub fn hovering() -> bool {
    HOVERING.load(Ordering::Relaxed)
}

/// Called (on the clipboard thread) when a drop arrives or a drag enters or leaves, e.g. to wake
/// the UI. Only the first call takes effect.
pub fn set_drop_notify(f: impl Fn() + Send + Sync + 'static) {
    let _ = NOTIFY.set(Box::new(f));
}

fn notify() {
    if let Some(f) = NOTIFY.get() {
        f();
    }
}

pub(crate) fn set_hovering(on: bool) {
    if HOVERING.swap(on, Ordering::Relaxed) != on {
        notify();
    }
}

pub(crate) fn push(paths: Vec<PathBuf>) {
    if paths.is_empty() {
        return;
    }
    DROPPED.lock().unwrap().extend(paths);
    notify();
}

/// The local files in a `text/uri-list` (RFC 2483): one URI per line, `#` lines are comments.
pub(crate) fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_path)
        .collect()
}

/// `file:///a%20b` or `file://localhost/a%20b` as a path; other hosts and schemes aren't local.
fn file_path(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    let path = match rest.find('/') {
        Some(0) => rest,
        Some(i) if &rest[..i] == "localhost" => &rest[i..],
        _ => return None,
    };
    Some(PathBuf::from(OsString::from_vec(percent_decode(path.as_bytes()))))
}

fn percent_decode(s: &[u8]) -> Vec<u8> {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        if s[i] == b'%'
            && let (Some(h), Some(l)) = (s.get(i + 1).and_then(|&b| hex(b)), s.get(i + 2).and_then(|&b| hex(b)))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(s[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_become_local_paths() {
        let list = b"# from a file manager\r\nfile:///home/a/My%20Pack.mrpack\r\nfile://localhost/tmp/x.zip\r\nfile://other/y\r\nhttps://example.com/z\r\n";
        assert_eq!(parse_uri_list(list), vec![PathBuf::from("/home/a/My Pack.mrpack"), PathBuf::from("/tmp/x.zip")]);
        assert_eq!(parse_uri_list(b"file:///a%zz%41"), vec![PathBuf::from("/a%zzA")]);
    }
}
