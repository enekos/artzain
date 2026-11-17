//! Every libc call artzain makes, behind a safe API. This is the only module
//! allowed to use `unsafe`; callers never need it.

#![allow(unsafe_code, reason = "the one module that wraps libc")]

use crate::manifest::Limits;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;

const MAX_PASSWD_BUF: usize = 1 << 20;

/// Send `signal` to a single process. Refuses pid <= 0, which `kill(2)` would
/// otherwise widen to a process group or to every process we may signal.
pub fn signal_pid(pid: i32, signal: i32) -> io::Result<()> {
    if pid <= 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("refusing to signal pid {pid}"),
        ));
    }
    // SAFETY: kill(2) has no memory-safety preconditions; pid > 0 targets
    // exactly one process.
    if unsafe { libc::kill(pid, signal) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Send `signal` to every process in group `pgid`. Groups 0 and 1 and our own
/// group are refused: those would hit artzain itself or init.
pub fn signal_group(pgid: i32, signal: i32) {
    if pgid <= 1 || pgid == own_pgid() {
        return;
    }
    // SAFETY: kill(2) has no memory-safety preconditions; -pgid with pgid > 1
    // targets exactly that process group.
    unsafe { libc::kill(-pgid, signal) };
}

/// True if the process exists, including when we lack permission to signal it.
pub fn pid_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission check.
    let rc = unsafe { libc::kill(pid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// True if the process group exists, including when we lack permission to
/// signal it.
pub fn group_alive(pgid: i32) -> bool {
    if pgid <= 1 {
        return false;
    }
    // SAFETY: signal 0 performs only the existence and permission check.
    let rc = unsafe { libc::kill(-pgid, 0) };
    rc == 0 || io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The process group `pid` currently belongs to.
pub fn pgid_of(pid: i32) -> Option<i32> {
    if pid <= 0 {
        return None;
    }
    // SAFETY: getpgid(2) has no memory-safety preconditions.
    let pgid = unsafe { libc::getpgid(pid) };
    (pgid > 0).then_some(pgid)
}

fn own_pgid() -> i32 {
    // SAFETY: getpgrp(2) has no preconditions and cannot fail.
    unsafe { libc::getpgrp() }
}

pub fn euid() -> u32 {
    // SAFETY: geteuid(2) has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

/// Non-blocking `flock(2)`. `Ok(false)` means another open file description
/// holds a conflicting lock.
pub fn try_flock(file: &File, exclusive: bool) -> io::Result<bool> {
    let op = if exclusive {
        libc::LOCK_EX
    } else {
        libc::LOCK_SH
    };
    // SAFETY: the fd is owned by `file`, which outlives the call.
    if unsafe { libc::flock(file.as_raw_fd(), op | libc::LOCK_NB) } == 0 {
        return Ok(true);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
        Ok(false)
    } else {
        Err(err)
    }
}

/// Resolve a user name to `(uid, primary_gid)`.
pub fn lookup_user(name: &str) -> Option<(u32, u32)> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0u8; 2048];
    loop {
        // SAFETY: passwd is a plain C struct; all-zero is a valid value that
        // getpwnam_r overwrites.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: every pointer is valid for the call and `buf.len()` is the
        // true length of `buf`; the strings in `pwd` point into `buf`, which
        // we never read.
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_PASSWD_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        return Some((pwd.pw_uid, pwd.pw_gid));
    }
}

/// Resolve a group name to a gid.
pub fn lookup_group(name: &str) -> Option<u32> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0u8; 2048];
    loop {
        // SAFETY: group is a plain C struct; all-zero is a valid value that
        // getgrnam_r overwrites.
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut result: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as in `lookup_user`.
        let rc = unsafe {
            libc::getgrnam_r(
                cname.as_ptr(),
                &mut grp,
                buf.as_mut_ptr().cast(),
                buf.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_PASSWD_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        return Some(grp.gr_gid);
    }
}

/// Install a hook that, in the child between fork and exec, applies `limits`
/// and then drops to `gid` and `uid`. Any failure aborts the exec, so a child
/// never runs with more privilege than the manifest asked for.
pub fn drop_privileges_before_exec(
    cmd: &mut tokio::process::Command,
    uid: Option<u32>,
    gid: Option<u32>,
    limits: Option<Limits>,
) {
    let nofile = limits.as_ref().and_then(|l| l.open_files);
    let memory = limits
        .as_ref()
        .and_then(|l| l.memory_mb)
        .map(|mb| mb.saturating_mul(1024 * 1024));
    let hook = move || -> io::Result<()> {
        if let Some(n) = nofile {
            set_rlimit(libc::RLIMIT_NOFILE, n)?;
        }
        if let Some(bytes) = memory {
            set_rlimit(libc::RLIMIT_AS, bytes)?;
        }
        if let Some(gid) = gid {
            clear_supplementary_groups()?;
            set_gid(gid)?;
        }
        if let Some(uid) = uid {
            set_uid(uid)?;
        }
        Ok(())
    };
    // SAFETY: the hook runs after fork in a possibly multi-threaded parent, so
    // it must be async-signal-safe. It captures only `Copy` integers, does not
    // allocate on the success path, and calls only setrlimit, setgroups,
    // setgid and setuid, all async-signal-safe.
    unsafe { cmd.pre_exec(hook) };
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
type Resource = libc::c_int;

fn set_rlimit(resource: Resource, value: u64) -> io::Result<()> {
    let rl = libc::rlimit {
        rlim_cur: value as libc::rlim_t,
        rlim_max: value as libc::rlim_t,
    };
    // SAFETY: `rl` is a valid rlimit for the duration of the call.
    if unsafe { libc::setrlimit(resource, &rl) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn clear_supplementary_groups() -> io::Result<()> {
    // SAFETY: a zero-length list with a null pointer is the documented way to
    // drop every supplementary group.
    if unsafe { libc::setgroups(0, std::ptr::null()) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_gid(gid: u32) -> io::Result<()> {
    // SAFETY: setgid(2) has no memory-safety preconditions.
    if unsafe { libc::setgid(gid) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn set_uid(uid: u32) -> io::Result<()> {
    // SAFETY: setuid(2) has no memory-safety preconditions.
    if unsafe { libc::setuid(uid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if uid != 0 && unsafe { libc::setuid(0) } == 0 {
        return Err(io::Error::from(io::ErrorKind::PermissionDenied));
    }
    Ok(())
}

/// Seconds since the epoch at which the system booted, where we can tell.
pub fn boot_time() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/stat")
            .ok()?
            .lines()
            .find_map(|l| l.strip_prefix("btime "))
            .and_then(|v| v.trim().parse().ok())
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}
