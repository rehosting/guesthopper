//! The `fs` verb: a small file-service RPC on the command channel.
//!
//! This is what the host-side SSH gateway's SFTP/scp support is built on. Like
//! the console, the guest runs no sshd/sftp-server and needs no userland tools
//! (`cat`, `stat`, ...): the SFTP protocol is terminated on the host and only
//! these plain file operations cross into the guest, executed natively here.
//!
//! After a `{"verb":"fs"}` REQUEST the session becomes a request/response loop:
//!
//!   host  -> guest  `FRAME_FS_REQ`   payload = `[hdr_len: u32 be][JSON hdr][data]`
//!   guest -> host   `FRAME_FS_RESP`  payload = `[hdr_len: u32 be][JSON hdr][data]`
//!
//! Requests are served strictly in order, one response per request, so a client
//! may pipeline (send several requests before reading) and match responses
//! FIFO. The JSON header of a request is `{"op": ..., ...}`; of a response
//! `{"ok": true, ...}` or `{"ok": false, "kind": ..., "errno": N, "message": ...}`.
//! `kind` is a portable error class ("noent", "perm", "exist", "invalid",
//! "unsupported", "fail") -- raw errno numbers differ between guest
//! architectures, so the host must not interpret `errno` beyond logging it.
//!
//! Paths are hex-encoded bytes in the header (firmware filenames are not
//! reliably UTF-8, and JSON strings must be); file contents travel raw in the
//! `data` tail (`write` request, `read` response). `PING` frames keep the
//! session's idle timeout fresh exactly as for `exec`/`open-pty`.

use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::fs::{self, File, ReadDir};
use std::io::{self, Write};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{DirBuilderExt, FileExt, MetadataExt, PermissionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::AsyncRead;
use tokio::time::{timeout, Duration};

use crate::frame;
use crate::session::{send, send_error, Cancel, Tx};

/// Largest `read` served in one response. Matches the exec path's chunk so the
/// bounded outbound queue has the same worst-case footprint; a client wanting
/// more issues further (pipelined) reads.
pub const FS_READ_MAX: usize = 32 * 1024;
/// Open file + directory handles per `fs` session. A leaked-handle client can't
/// exhaust the agent's fds; SFTP clients use a handful at a time.
const MAX_HANDLES: usize = 64;
/// Directory entries returned per `readdir` call (bounds response size for a
/// huge directory; the client keeps calling until `eof`).
const READDIR_MAX: usize = 128;
/// Backstop for a single file operation wedged in the kernel (a read from a
/// half-implemented emulated device, a stuck flash driver). The op runs on a
/// blocking thread so it never stalls other sessions, but without a bound this
/// session would hold its slot forever.
const FS_OP_TIMEOUT: Duration = Duration::from_secs(120);
/// File operations that may be on blocking threads at once, across every
/// session. A session whose op times out (or whose client vanishes mid-op)
/// ends and frees its slot, but the op's thread and its whole handle table live
/// on until the syscall returns -- and a new session can then start another.
/// Without a global bound, a guest with a wedged filesystem or device could
/// accumulate such threads and descriptors without limit. The permit is held by
/// the job itself, so an abandoned job keeps counting until it really ends.
/// Sized to the default session cap: healthy sessions (one op each at a time)
/// never wait on it.
pub const FS_MAX_JOBS: usize = 64;
/// Longest a request waits for a job permit before being refused as busy.
const FS_ADMIT_TIMEOUT: Duration = Duration::from_secs(10);

static FS_JOBS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(FS_MAX_JOBS);

/// Held by a running job; releasing it frees one of the FS_MAX_JOBS slots.
type JobPermit = tokio::sync::SemaphorePermit<'static>;

/// Wait up to `admit` for a job permit. `None` means the agent is saturated.
/// The wait is async, so a session that goes away while queued simply stops
/// waiting and never starts its job.
async fn admit_job(admit: Duration) -> Option<JobPermit> {
    match timeout(admit, FS_JOBS.acquire()).await {
        Ok(Ok(permit)) => Some(permit),
        // Elapsed, or the semaphore closed (it never is).
        _ => None,
    }
}

/// Run `f` on a blocking thread. The permit moves into the job and is released
/// only when `f` returns -- even if the caller has stopped waiting for it.
fn spawn_job<T, F>(permit: JobPermit, f: F) -> tokio::task::JoinHandle<T>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FsReq {
    op: String,
    /// Hex-encoded path (the subject of the op).
    path: Option<String>,
    /// Hex-encoded second path (rename/link destination, symlink location).
    to: Option<String>,
    /// File or directory handle from `open`/`opendir`.
    h: Option<u32>,
    off: u64,
    len: u32,
    max: Option<u32>,
    /// stat/setstat: follow a trailing symlink (default true).
    follow: Option<bool>,
    // open flags
    read: bool,
    write: bool,
    append: bool,
    create: bool,
    trunc: bool,
    excl: bool,
    /// rename: overwrite an existing destination (POSIX rename semantics).
    replace: bool,
    // open/mkdir permission bits; setstat attributes.
    mode: Option<u32>,
    uid: Option<u32>,
    gid: Option<u32>,
    atime: Option<i64>,
    mtime: Option<i64>,
    size: Option<u64>,
}

enum Handle {
    File { file: File, append: bool },
    Dir(ReadDir),
}

#[derive(Default)]
struct FsState {
    handles: HashMap<u32, Handle>,
    next: u32,
}

type OpResult = io::Result<(Value, Vec<u8>)>;

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg.to_string())
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> io::Result<Vec<u8>> {
    fn nibble(c: u8) -> io::Result<u8> {
        match c {
            b'0'..=b'9' => Ok(c - b'0'),
            b'a'..=b'f' => Ok(c - b'a' + 10),
            b'A'..=b'F' => Ok(c - b'A' + 10),
            _ => Err(invalid("path is not valid hex")),
        }
    }
    let raw = s.as_bytes();
    if raw.len() % 2 != 0 {
        return Err(invalid("path is not valid hex"));
    }
    raw.chunks(2)
        .map(|p| Ok((nibble(p[0])? << 4) | nibble(p[1])?))
        .collect()
}

fn decode_path(field: &Option<String>, what: &str) -> io::Result<PathBuf> {
    let s = field
        .as_ref()
        .ok_or_else(|| invalid(&format!("missing {what}")))?;
    let bytes = unhex(s)?;
    if bytes.is_empty() {
        return Err(invalid(&format!("empty {what}")));
    }
    Ok(PathBuf::from(OsString::from_vec(bytes)))
}

fn cpath(p: &Path) -> io::Result<CString> {
    CString::new(p.as_os_str().as_bytes()).map_err(|_| invalid("path contains a NUL byte"))
}

fn attrs(m: &fs::Metadata) -> Value {
    json!({
        "mode": m.mode(),
        "size": m.size(),
        "uid": m.uid(),
        "gid": m.gid(),
        "atime": m.atime(),
        "mtime": m.mtime(),
        "nlink": m.nlink(),
    })
}

// The casts are no-ops on 64-bit hosts but needed where the fields are 32-bit.
#[allow(clippy::unnecessary_cast)]
fn statvfs_json(s: &libc::statvfs) -> Value {
    json!({
        "bsize": s.f_bsize as u64,
        "frsize": s.f_frsize as u64,
        "blocks": s.f_blocks as u64,
        "bfree": s.f_bfree as u64,
        "bavail": s.f_bavail as u64,
        "files": s.f_files as u64,
        "ffree": s.f_ffree as u64,
        "favail": s.f_favail as u64,
        "fsid": s.f_fsid as u64,
        "flag": s.f_flag as u64,
        "namemax": s.f_namemax as u64,
    })
}

fn check(rc: libc::c_int) -> io::Result<()> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// `[atime, mtime]` for utimensat/futimens; an absent one is left untouched.
fn timespecs(atime: Option<i64>, mtime: Option<i64>) -> [libc::timespec; 2] {
    let one = |t: Option<i64>| {
        // Zeroed rather than a struct literal: some targets' `timespec` carries
        // private padding fields.
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        match t {
            // `as _` (not `as libc::time_t`): the alias is flagged deprecated
            // on 32-bit musl, where it is still 32 bits wide. Whatever width
            // this libc declares is the one its utimensat/futimens expect.
            Some(secs) => ts.tv_sec = secs as _,
            None => ts.tv_nsec = libc::UTIME_OMIT as _,
        }
        ts
    };
    [one(atime), one(mtime)]
}

/// Resolve `p` to an absolute path. Unlike `canonicalize`, a path whose tail
/// does not exist yet still resolves (SFTP clients realpath an upload
/// destination before creating it). The part that exists is resolved by the
/// kernel one component at a time -- so symlinks and `..` behave exactly as
/// they would for a real lookup -- and only components that do not exist are
/// normalized lexically (nothing there can be a symlink).
fn realpath(p: &Path) -> io::Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()?.join(p)
    };
    if let Ok(real) = fs::canonicalize(&abs) {
        return Ok(real);
    }
    let mut out = PathBuf::from("/");
    // How many components at the end of `out` were appended lexically because
    // they do not exist. While it is zero, `out` is a real, resolved path and
    // the next component is looked up through the kernel; a `..` that removes
    // the last missing component puts us back in that state, so anything after
    // it (symlinks included) resolves for real again.
    let mut missing = 0usize;
    for c in abs.components() {
        match c {
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
            Component::ParentDir if missing > 0 => {
                out.pop();
                missing -= 1;
            }
            Component::Normal(name) if missing > 0 => {
                out.push(name);
                missing += 1;
            }
            Component::ParentDir | Component::Normal(_) => match fs::canonicalize(out.join(c)) {
                Ok(real) => out = real,
                Err(e) if e.kind() == io::ErrorKind::NotFound => match c {
                    Component::Normal(name) => {
                        out.push(name);
                        missing = 1;
                    }
                    // `..` of a directory that vanished under us.
                    _ => {
                        out.pop();
                    }
                },
                // ENOTDIR, EACCES, ELOOP...: a real lookup would fail too.
                Err(e) => return Err(e),
            },
        }
    }
    Ok(out)
}

/// open(2) with flags built directly rather than through `OpenOptions`, which
/// refuses combinations that are legal for POSIX and SFTP clients
/// (append+truncate, create without write). Always non-blocking and never a
/// controlling terminal: an open of a FIFO or tty with no peer must not park
/// the blocking thread, which nothing could then interrupt.
///
/// Handles are served with pread/pwrite, so non-seekable files (FIFOs, ttys,
/// sockets) open but cannot be read or written -- only regular files and
/// seekable devices transfer.
fn open_file(p: &Path, req: &FsReq, extra_flags: libc::c_int) -> io::Result<File> {
    let writable = req.write || req.append;
    if req.trunc && !writable {
        // Linux would truncate on O_RDONLY|O_TRUNC; never do that by surprise.
        return Err(invalid("truncate requires write access"));
    }
    let mut flags = match (req.read, writable) {
        (true, true) => libc::O_RDWR,
        (false, true) => libc::O_WRONLY,
        (_, false) => libc::O_RDONLY,
    };
    for (on, flag) in [
        (req.append, libc::O_APPEND),
        (req.create, libc::O_CREAT),
        (req.create && req.excl, libc::O_EXCL),
        (req.trunc, libc::O_TRUNC),
    ] {
        if on {
            flags |= flag;
        }
    }
    flags |= libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC | libc::O_LARGEFILE | extra_flags;
    let c = cpath(p)?;
    let mode = req.mode.unwrap_or(0o666) & 0o7777;
    // SAFETY: `c` is a valid NUL-terminated path; a non-negative return is a
    // fresh descriptor we own.
    let fd = unsafe { libc::open(c.as_ptr(), flags, mode as libc::c_uint) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

/// rename(2) that fails with EEXIST instead of replacing `to`. Atomic where
/// the kernel has renameat2(RENAME_NOREPLACE) (Linux >= 3.15 and a filesystem
/// that supports it); otherwise falls back to check-then-rename, which can
/// lose a race with a concurrent creator but is the best an old kernel offers.
fn rename_noreplace(from: &Path, to: &Path) -> io::Result<()> {
    const RENAME_NOREPLACE: libc::c_uint = 1;
    let (cf, ct) = (cpath(from)?, cpath(to)?);
    // SAFETY: valid NUL-terminated paths; raw syscall because not every libc
    // this builds against exports a renameat2 wrapper.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            libc::AT_FDCWD,
            cf.as_ptr(),
            libc::AT_FDCWD,
            ct.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if rc == 0 {
        return Ok(());
    }
    let e = io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::ENOSYS) | Some(libc::EINVAL) => {
            if fs::symlink_metadata(to).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "destination already exists",
                ));
            }
            fs::rename(from, to)
        }
        _ => Err(e),
    }
}

impl FsState {
    /// Refuse a new handle when the table is full. Called *before* the open it
    /// guards: an open can create or truncate its target, and a request that is
    /// going to be rejected must not have done that first.
    fn check_capacity(&self) -> io::Result<()> {
        if self.handles.len() >= MAX_HANDLES {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("too many open handles (max {MAX_HANDLES})"),
            ));
        }
        Ok(())
    }

    fn insert(&mut self, h: Handle) -> io::Result<u32> {
        self.check_capacity()?;
        loop {
            self.next = self.next.wrapping_add(1);
            if !self.handles.contains_key(&self.next) {
                break;
            }
        }
        self.handles.insert(self.next, h);
        Ok(self.next)
    }

    fn file(&self, req: &FsReq) -> io::Result<(&File, bool)> {
        match req.h.and_then(|h| self.handles.get(&h)) {
            Some(Handle::File { file, append }) => Ok((file, *append)),
            _ => Err(invalid("not an open file handle")),
        }
    }

    fn op(&mut self, req: &FsReq, data: &[u8]) -> OpResult {
        let none = Vec::new();
        match req.op.as_str() {
            "stat" => {
                let p = decode_path(&req.path, "path")?;
                let m = if req.follow.unwrap_or(true) {
                    fs::metadata(&p)?
                } else {
                    fs::symlink_metadata(&p)?
                };
                Ok((json!({ "attrs": attrs(&m) }), none))
            }
            "fstat" => {
                let (f, _) = self.file(req)?;
                Ok((json!({ "attrs": attrs(&f.metadata()?) }), none))
            }
            "open" => {
                let p = decode_path(&req.path, "path")?;
                self.check_capacity()?;
                let file = open_file(&p, req, 0)?;
                let h = self.insert(Handle::File { file, append: req.append })?;
                Ok((json!({ "h": h }), none))
            }
            "read" => {
                let (f, _) = self.file(req)?;
                let want = (req.len as usize).min(FS_READ_MAX);
                let mut buf = vec![0u8; want];
                let mut got = 0;
                while got < want {
                    match f.read_at(&mut buf[got..], req.off + got as u64) {
                        Ok(0) => break,
                        Ok(n) => got += n,
                        Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                        // Report an error only if nothing was read; otherwise
                        // hand back the short read and let the next call see it.
                        Err(e) if got == 0 => return Err(e),
                        Err(_) => break,
                    }
                }
                buf.truncate(got);
                Ok((json!({ "eof": got < want }), buf))
            }
            "write" => {
                let (f, append) = self.file(req)?;
                if append {
                    let mut f = f;
                    f.write_all(data)?;
                } else {
                    f.write_all_at(data, req.off)?;
                }
                Ok((json!({}), none))
            }
            "fsync" => {
                let (f, _) = self.file(req)?;
                f.sync_all()?;
                Ok((json!({}), none))
            }
            "close" => match req.h.and_then(|h| self.handles.remove(&h)) {
                Some(_) => Ok((json!({}), none)),
                None => Err(invalid("not an open handle")),
            },
            "setstat" => {
                self.setstat(req)?;
                Ok((json!({}), none))
            }
            "opendir" => {
                let p = decode_path(&req.path, "path")?;
                self.check_capacity()?;
                let h = self.insert(Handle::Dir(fs::read_dir(&p)?))?;
                Ok((json!({ "h": h }), none))
            }
            "readdir" => {
                let rd = match req.h.and_then(|h| self.handles.get_mut(&h)) {
                    Some(Handle::Dir(rd)) => rd,
                    _ => return Err(invalid("not an open directory handle")),
                };
                let max = (req.max.unwrap_or(READDIR_MAX as u32) as usize).clamp(1, READDIR_MAX);
                let mut entries = Vec::new();
                let mut eof = false;
                while entries.len() < max {
                    match rd.next() {
                        None => {
                            eof = true;
                            break;
                        }
                        Some(Err(e)) => return Err(e),
                        Some(Ok(ent)) => {
                            // DirEntry::metadata is an lstat. An entry that
                            // vanished mid-listing just reports no attrs.
                            let a = ent.metadata().map(|m| attrs(&m)).unwrap_or(Value::Null);
                            entries.push(json!({
                                "name": hex(ent.file_name().as_bytes()),
                                "attrs": a,
                            }));
                        }
                    }
                }
                Ok((json!({ "entries": entries, "eof": eof }), none))
            }
            "mkdir" => {
                let p = decode_path(&req.path, "path")?;
                fs::DirBuilder::new()
                    .mode(req.mode.unwrap_or(0o777) & 0o7777)
                    .create(&p)?;
                Ok((json!({}), none))
            }
            "rmdir" => {
                fs::remove_dir(decode_path(&req.path, "path")?)?;
                Ok((json!({}), none))
            }
            "remove" => {
                fs::remove_file(decode_path(&req.path, "path")?)?;
                Ok((json!({}), none))
            }
            "rename" => {
                let from = decode_path(&req.path, "path")?;
                let to = decode_path(&req.to, "destination path")?;
                // SFTP's plain rename must not clobber; posix-rename may.
                if req.replace {
                    fs::rename(&from, &to)?;
                } else {
                    rename_noreplace(&from, &to)?;
                }
                Ok((json!({}), none))
            }
            "readlink" => {
                let target = fs::read_link(decode_path(&req.path, "path")?)?;
                Ok((json!({ "target": hex(target.as_os_str().as_bytes()) }), none))
            }
            "symlink" => {
                // `path` is the link's target (stored verbatim), `to` the link.
                let target = decode_path(&req.path, "path")?;
                let link = decode_path(&req.to, "link path")?;
                std::os::unix::fs::symlink(&target, &link)?;
                Ok((json!({}), none))
            }
            "link" => {
                let existing = decode_path(&req.path, "path")?;
                let link = decode_path(&req.to, "link path")?;
                fs::hard_link(&existing, &link)?;
                Ok((json!({}), none))
            }
            "realpath" => {
                let real = realpath(&decode_path(&req.path, "path")?)?;
                Ok((json!({ "path": hex(real.as_os_str().as_bytes()) }), none))
            }
            "statvfs" => {
                let c = cpath(&decode_path(&req.path, "path")?)?;
                let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: `c` is a valid NUL-terminated path, `s` a valid out-pointer.
                check(unsafe { libc::statvfs(c.as_ptr(), &mut s) })?;
                Ok((statvfs_json(&s), none))
            }
            "fstatvfs" => {
                // By handle, not by the path it was opened with: the file may
                // have been renamed or unlinked since, or the path may now name
                // something on another filesystem.
                let (f, _) = self.file(req)?;
                let mut s: libc::statvfs = unsafe { std::mem::zeroed() };
                // SAFETY: a live descriptor and a valid out-pointer.
                check(unsafe { libc::fstatvfs(f.as_raw_fd(), &mut s) })?;
                Ok((statvfs_json(&s), none))
            }
            // Name the op, but only a short prefix of it: the string is
            // client-controlled and up to a whole inbound frame long, and an
            // error that echoed it (escaped, so several times larger) would let
            // a client that never reads park tens of MiB per queued response.
            other => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported fs op: {:?}", other.chars().take(32).collect::<String>()),
            )),
        }
    }

    /// Apply whichever of size/mode/owner/times the request carries, to an open
    /// handle (`h`) or a path. Order matters: size before times (truncating
    /// updates mtime) and chown before chmod (chown clears setuid/setgid bits
    /// the chmod may be about to set). Not transactional: like sftp-server, a
    /// failure part-way leaves the earlier attributes applied.
    fn setstat(&mut self, req: &FsReq) -> io::Result<()> {
        if req.h.is_some() {
            let (f, _) = self.file(req)?;
            let fd = f.as_raw_fd();
            if let Some(size) = req.size {
                f.set_len(size)?;
            }
            if req.uid.is_some() || req.gid.is_some() {
                // SAFETY: fd is a live descriptor; u32::MAX (-1) leaves an id as is.
                check(unsafe {
                    libc::fchown(fd, req.uid.unwrap_or(u32::MAX), req.gid.unwrap_or(u32::MAX))
                })?;
            }
            if let Some(mode) = req.mode {
                f.set_permissions(fs::Permissions::from_mode(mode & 0o7777))?;
            }
            if req.atime.is_some() || req.mtime.is_some() {
                let ts = timespecs(req.atime, req.mtime);
                // SAFETY: fd is live; ts points at two valid timespecs.
                check(unsafe { libc::futimens(fd, ts.as_ptr()) })?;
            }
            return Ok(());
        }

        let p = decode_path(&req.path, "path")?;
        let c = cpath(&p)?;
        let follow = req.follow.unwrap_or(true);
        if let Some(size) = req.size {
            // Open through open_file so this can't block (a FIFO with no reader
            // fails with ENXIO instead of parking the thread), and with
            // O_NOFOLLOW for a no-follow request so a symlink is refused rather
            // than its target truncated.
            let how = FsReq { write: true, ..FsReq::default() };
            let nofollow = if follow { 0 } else { libc::O_NOFOLLOW };
            open_file(&p, &how, nofollow)?.set_len(size)?;
        }
        if req.uid.is_some() || req.gid.is_some() {
            let (uid, gid) = (req.uid.unwrap_or(u32::MAX), req.gid.unwrap_or(u32::MAX));
            // SAFETY: `c` is a valid NUL-terminated path.
            check(unsafe {
                if follow {
                    libc::chown(c.as_ptr(), uid, gid)
                } else {
                    libc::lchown(c.as_ptr(), uid, gid)
                }
            })?;
        }
        if let Some(mode) = req.mode {
            // Linux has no lchmod: a symlink's own mode can't be changed, so a
            // no-follow chmod of a symlink is a no-op rather than a chmod of
            // whatever it points at.
            let is_link = fs::symlink_metadata(&p)?.file_type().is_symlink();
            if follow || !is_link {
                fs::set_permissions(&p, fs::Permissions::from_mode(mode & 0o7777))?;
            }
        }
        if req.atime.is_some() || req.mtime.is_some() {
            let ts = timespecs(req.atime, req.mtime);
            let flags = if follow { 0 } else { libc::AT_SYMLINK_NOFOLLOW };
            // SAFETY: valid path and timespec pointers.
            check(unsafe { libc::utimensat(libc::AT_FDCWD, c.as_ptr(), ts.as_ptr(), flags) })?;
        }
        Ok(())
    }

    /// Serve one request payload, returning the encoded response payload. Never
    /// fails: every error becomes an `ok: false` response so the response
    /// stream stays one-to-one with the requests.
    fn handle(&mut self, payload: &[u8]) -> Vec<u8> {
        let result = split_payload(payload).and_then(|(hdr, data)| {
            let req: FsReq = serde_json::from_slice(hdr)
                .map_err(|e| invalid(&format!("invalid fs request json: {e}")))?;
            self.op(&req, data)
        });
        match result {
            Ok((mut hdr, data)) => {
                hdr["ok"] = Value::Bool(true);
                encode_payload(&hdr, &data)
            }
            Err(e) => error_payload(&e),
        }
    }
}

/// The `ok: false` response payload for a failed request.
fn error_payload(e: &io::Error) -> Vec<u8> {
    let hdr = json!({
        "ok": false,
        "kind": error_kind(e),
        "errno": e.raw_os_error().unwrap_or(0),
        "message": e.to_string(),
    });
    encode_payload(&hdr, &[])
}

fn error_kind(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::NotFound => "noent",
        io::ErrorKind::PermissionDenied => "perm",
        io::ErrorKind::AlreadyExists => "exist",
        io::ErrorKind::InvalidInput => "invalid",
        io::ErrorKind::Unsupported => "unsupported",
        _ => "fail",
    }
}

/// Split `[hdr_len: u32 be][hdr][data]` into `(hdr, data)`.
fn split_payload(payload: &[u8]) -> io::Result<(&[u8], &[u8])> {
    if payload.len() < 4 {
        return Err(invalid("fs frame too short"));
    }
    let n = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let rest = &payload[4..];
    if n > rest.len() {
        return Err(invalid("fs frame header length exceeds payload"));
    }
    Ok(rest.split_at(n))
}

fn encode_payload(hdr: &Value, data: &[u8]) -> Vec<u8> {
    let hdr = serde_json::to_vec(hdr).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    let mut out = Vec::with_capacity(4 + hdr.len() + data.len());
    out.extend_from_slice(&(hdr.len() as u32).to_be_bytes());
    out.extend_from_slice(&hdr);
    out.extend_from_slice(data);
    out
}

/// Run the `fs` request loop until the client disconnects (or goes silent past
/// `idle_timeout`). Dropping the state on the way out closes every handle the
/// client left open.
pub(crate) async fn serve<R>(
    reader: &mut R,
    tx: &Tx,
    cancel: &Cancel,
    idle_timeout: Duration,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    let mut state = FsState::default();
    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.wait() => break,
            r = timeout(idle_timeout, frame::read_frame(reader)) => r,
        };
        let f = match next {
            // No frame (not even a PING) within idle_timeout: the peer is gone.
            Err(_elapsed) => {
                cancel.fire();
                break;
            }
            Ok(Ok(Some(f))) => f,
            Ok(Ok(None)) | Ok(Err(_)) => break,
        };
        if f.ftype != frame::FRAME_FS_REQ {
            continue; // PING / unknown frames: just liveness
        }

        // File I/O is synchronous; on this single-threaded runtime it would
        // stall every other session (the console included) for its duration, so
        // each op runs on a blocking thread. The state moves in and back out.
        //
        // A blocking thread cannot be interrupted. If cancel or the op timeout
        // wins the select below, the session ends and frees its slot at once,
        // but the thread -- and the handles in the state it holds -- live until
        // the syscall returns. That is why every open here is non-blocking: a
        // regular-file operation always returns, and anything that could wait
        // on a peer forever fails fast instead. What remains -- a regular-file
        // op stuck in a wedged driver -- is bounded globally by FS_MAX_JOBS.
        //
        let permit = tokio::select! {
            biased;
            _ = cancel.wait() => break,
            p = admit_job(FS_ADMIT_TIMEOUT.min(idle_timeout)) => p,
        };
        let permit = match permit {
            Some(p) => p,
            None => {
                // Saturated: refuse this one request (keeping the response
                // stream one-to-one) and leave the session and its handles be.
                let busy = io::Error::new(
                    io::ErrorKind::Other,
                    "agent busy: too many file operations in flight",
                );
                send(tx, frame::FRAME_FS_RESP, error_payload(&busy)).await;
                continue;
            }
        };
        let job = spawn_job(permit, move || {
            let resp = state.handle(&f.payload);
            (state, resp)
        });
        let resp = tokio::select! {
            biased;
            _ = cancel.wait() => break,
            r = timeout(FS_OP_TIMEOUT, job) => match r {
                Ok(Ok((st, resp))) => {
                    state = st;
                    resp
                }
                Ok(Err(e)) => {
                    send_error(tx, &format!("fs operation failed: {e}")).await;
                    break;
                }
                Err(_elapsed) => {
                    send_error(tx, "fs operation timed out (wedged in the guest kernel?)").await;
                    break;
                }
            },
        };
        send(tx, frame::FRAME_FS_RESP, resp).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    // These tests drain the process-global FS_JOBS, so they must not overlap.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    #[tokio::test]
    async fn abandoned_job_holds_its_permit_until_it_actually_finishes() {
        let _serial = SERIAL.lock().await;
        let before = FS_JOBS.available_permits();
        let (release, gate) = mpsc::channel::<()>();
        let permit = admit_job(Duration::from_secs(5)).await.expect("permit");
        let job = spawn_job(permit, move || {
            let _ = gate.recv(); // a wedged syscall
        });
        // The session gives up on the job (op timeout / client gone)...
        drop(job);
        tokio::time::sleep(Duration::from_millis(100)).await;
        // ...but the thread is still there, so it must still count.
        assert_eq!(FS_JOBS.available_permits(), before - 1);

        release.send(()).unwrap();
        for _ in 0..100 {
            if FS_JOBS.available_permits() == before {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("permit was not released when the job finished");
    }

    #[tokio::test]
    async fn saturated_agent_refuses_requests_but_keeps_the_session() {
        let _serial = SERIAL.lock().await;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(u8, Vec<u8>)>(8);
        let (host, guest) = tokio::io::duplex(1 << 16);
        let (mut g_rd, _g_wr) = tokio::io::split(guest);
        let (_h_rd, mut h_wr) = tokio::io::split(host);
        let cancel = Cancel::default();
        let request = encode_payload(&json!({ "op": "realpath", "path": "2f" }), &[]);

        // Every job slot is taken by lingering work.
        let all = FS_JOBS.acquire_many(FS_MAX_JOBS as u32).await.unwrap();
        frame::write_frame(&mut h_wr, frame::FRAME_FS_REQ, &request).await.unwrap();
        // idle_timeout doubles as the admission bound here; keep it short.
        let serving = serve(&mut g_rd, &tx, &cancel, Duration::from_millis(300));
        tokio::pin!(serving);
        let (ftype, payload) = tokio::select! {
            _ = &mut serving => panic!("session ended instead of answering"),
            r = rx.recv() => r.expect("a response"),
        };
        assert_eq!(ftype, frame::FRAME_FS_RESP);
        let (hdr, _) = split_payload(&payload).unwrap();
        let hdr: Value = serde_json::from_slice(hdr).unwrap();
        assert_eq!(hdr["ok"], false);
        assert!(hdr["message"].as_str().unwrap().contains("busy"), "{hdr}");

        // Once work drains, the same session serves the next request.
        drop(all);
        frame::write_frame(&mut h_wr, frame::FRAME_FS_REQ, &request).await.unwrap();
        let (_, payload) = tokio::select! {
            _ = &mut serving => panic!("session ended instead of answering"),
            r = rx.recv() => r.expect("a response"),
        };
        let hdr: Value = serde_json::from_slice(split_payload(&payload).unwrap().0).unwrap();
        assert_eq!(hdr["ok"], true, "{hdr}");
    }
}
