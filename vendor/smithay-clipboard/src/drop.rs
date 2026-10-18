//! Files dropped on the app's windows (added by Omacraft).
//!
//! winit has no drag and drop on Wayland, and compositors like Hyprland send drops only to a
//! client's first data device, which is this crate's. So the clipboard's data device takes
//! `text/uri-list` drops too and queues the paths here for the app to take.

use std::cell::RefCell;
use std::ffi::OsString;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use sctk::data_device_manager::ReadPipe;
use sctk::reexports::calloop::timer::{TimeoutAction, Timer};
use sctk::reexports::calloop::{LoopHandle, PostAction};

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

/// The most a drop's uri list may hold: a few thousand long paths fit easily.
pub(crate) const MAX_BYTES: usize = 1 << 20;
/// The most paths one drop may carry.
pub(crate) const MAX_PATHS: usize = 4096;
/// Reads of [`CHUNK`] bytes per wake-up, so a source that never stops sending can't keep the
/// clipboard thread from its other work (the pipe is level-triggered: the rest is read next turn).
pub(crate) const READS_PER_WAKE: usize = 16;
const CHUNK: usize = 4096;
/// How long a drop's source gets to send its list before the transfer is dropped.
pub(crate) const DEADLINE: Duration = Duration::from_secs(10);

/// How far a transfer has got.
#[derive(Debug, PartialEq)]
pub(crate) enum Progress {
    Pending,
    Done(Vec<PathBuf>),
    /// Too big, too many paths, or the pipe failed.
    Failed,
}

/// The uri list of one drop, as it arrives.
#[derive(Default)]
pub(crate) struct Transfer {
    content: Vec<u8>,
}

impl Transfer {
    /// Read what's ready from the (non-blocking) `r`, at most [`READS_PER_WAKE`] chunks.
    pub(crate) fn read(&mut self, r: &mut impl Read) -> Progress {
        let mut buf = [0; CHUNK];
        for _ in 0..READS_PER_WAKE {
            match r.read(&mut buf) {
                Ok(0) => {
                    let paths = parse_uri_list(&self.content);
                    return if paths.len() > MAX_PATHS { Progress::Failed } else { Progress::Done(paths) };
                },
                Ok(n) if self.content.len() + n > MAX_BYTES => return Progress::Failed,
                Ok(n) => self.content.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return Progress::Pending,
                Err(e) if e.kind() == ErrorKind::Interrupted => {},
                Err(_) => return Progress::Failed,
            }
        }
        Progress::Pending
    }
}

/// Read a drop's uri list from `pipe` (non-blocking already) on the event loop, and call `end`
/// exactly once: with the paths, or with `None` if the transfer failed, grew past the caps or
/// didn't finish within `deadline` (then the pipe is closed and the source forgotten).
pub(crate) fn receive<D: 'static>(
    handle: &LoopHandle<'static, D>,
    pipe: ReadPipe,
    deadline: Duration,
    end: impl FnOnce(Option<Vec<PathBuf>>) + 'static,
) {
    let end = Rc::new(RefCell::new(Some(Box::new(end) as Box<dyn FnOnce(Option<Vec<PathBuf>>)>)));
    let finish = |end: &Rc<RefCell<Option<Box<dyn FnOnce(Option<Vec<PathBuf>>)>>>>, r| {
        if let Some(f) = end.borrow_mut().take() {
            f(r);
        }
    };
    let mut transfer = Transfer::default();
    let on_pipe = end.clone();
    let Ok(token) = handle.insert_source(pipe, move |_, file, _| {
        let file = unsafe { file.get_mut() };
        match transfer.read(file) {
            Progress::Pending => PostAction::Continue,
            Progress::Done(paths) => {
                finish(&on_pipe, Some(paths));
                PostAction::Remove
            },
            Progress::Failed => {
                finish(&on_pipe, None);
                PostAction::Remove
            },
        }
    }) else {
        finish(&end, None);
        return;
    };
    let (h, on_timeout) = (handle.clone(), end.clone());
    let timeout = handle.insert_source(Timer::from_duration(deadline), move |_, _, _| {
        // Still waiting: drop the transfer, closing the pipe.
        if on_timeout.borrow().is_some() {
            h.remove(token);
            finish(&on_timeout, None);
        }
        TimeoutAction::Drop
    });
    if timeout.is_err() {
        // Without a deadline a source that never closes would be kept forever.
        handle.remove(token);
        finish(&end, None);
    }
}

/// The local files in a `text/uri-list` (RFC 2483): one URI per line, `#` lines are comments.
/// Stops one past [`MAX_PATHS`], which is enough to tell a list is too long.
pub(crate) fn parse_uri_list(bytes: &[u8]) -> Vec<PathBuf> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(file_path)
        .take(MAX_PATHS + 1)
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

    /// Reads forever, counting its reads.
    struct Endless(usize);

    impl Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            self.0 += 1;
            buf.fill(b'a');
            Ok(buf.len())
        }
    }

    #[test]
    fn transfers_are_capped_in_bytes_paths_and_reads_per_wake() {
        // A source that never stops: each wake-up reads a bounded amount, then it fails at the cap.
        let (mut t, mut r) = (Transfer::default(), Endless(0));
        let mut wakes = 0;
        while t.read(&mut r) == Progress::Pending {
            wakes += 1;
            assert!(r.0 <= wakes * READS_PER_WAKE, "{} reads in {wakes} wake-ups", r.0);
            assert!(wakes < 10_000, "never stopped");
        }
        assert!(t.content.len() <= MAX_BYTES);
        // Too many paths fail as a whole; the most allowed go through.
        let list = |n: usize| "file:///x\n".repeat(n).into_bytes();
        let done = |bytes: Vec<u8>| {
            let mut t = Transfer::default();
            let mut r = &bytes[..];
            loop {
                match t.read(&mut r) {
                    Progress::Pending => {},
                    p => break p,
                }
            }
        };
        assert_eq!(done(list(MAX_PATHS + 1)), Progress::Failed);
        assert!(matches!(done(list(MAX_PATHS)), Progress::Done(p) if p.len() == MAX_PATHS));
    }

    fn pipe() -> (ReadPipe, std::fs::File) {
        use std::os::fd::{FromRawFd, OwnedFd};
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        crate::state::set_non_blocking(fds[0]).unwrap();
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), std::fs::File::from_raw_fd(fds[1])) };
        (ReadPipe::from(r), w)
    }

    #[test]
    fn abandoned_and_endless_drops_leave_other_transfers_working() {
        use sctk::reexports::calloop::EventLoop;
        use std::io::Write;
        use std::time::Instant;
        let mut ev: EventLoop<'static, ()> = EventLoop::try_new().unwrap();
        let ends: Rc<RefCell<Vec<(&str, Option<Vec<PathBuf>>)>>> = Rc::default();
        let watch = |name: &'static str| {
            let ends = ends.clone();
            move |r| ends.borrow_mut().push((name, r))
        };
        // Never sends and never closes.
        let (quiet, mut quiet_w) = pipe();
        receive(&ev.handle(), quiet, Duration::from_millis(100), watch("quiet"));
        // Sends without end until its pipe is closed.
        let (endless, mut endless_w) = pipe();
        let writer = std::thread::spawn(move || while endless_w.write_all(&[b'a'; 4096]).is_ok() {});
        receive(&ev.handle(), endless, Duration::from_secs(10), watch("endless"));
        // An ordinary transfer meanwhile, like a paste.
        let (normal, mut normal_w) = pipe();
        normal_w.write_all(b"file:///ok\n").unwrap();
        drop(normal_w);
        receive(&ev.handle(), normal, Duration::from_secs(10), watch("normal"));

        let start = Instant::now();
        while ends.borrow().len() < 3 {
            assert!(start.elapsed() < Duration::from_secs(5), "transfers never ended: {:?}", ends.borrow());
            ev.dispatch(Duration::from_millis(10), &mut ()).unwrap();
        }
        let ends = ends.borrow();
        assert_eq!(ends[0], ("normal", Some(vec![PathBuf::from("/ok")])), "{ends:?}");
        assert!(ends.contains(&("endless", None)) && ends.contains(&("quiet", None)), "{ends:?}");
        // Both pipes were closed: the endless writer stopped, and the quiet one can't write.
        writer.join().unwrap();
        assert_eq!(quiet_w.write(b"x").unwrap_err().kind(), ErrorKind::BrokenPipe);
    }
}
