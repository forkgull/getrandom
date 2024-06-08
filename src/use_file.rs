//! Implementations that just need to read from a file
use crate::Error;
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};

#[cfg(not(all(any(target_os = "linux", target_os = "android"), feature = "rustix")))]
use crate::util_libc::{open_readonly, sys_fill_exact};
#[cfg(all(any(target_os = "linux", target_os = "android"), feature = "rustix"))]
use crate::util_rustix::{open_readonly, sys_fill_exact};

/// For all platforms, we use `/dev/urandom` rather than `/dev/random`.
/// For more information see the linked man pages in lib.rs.
///   - On Linux, "/dev/urandom is preferred and sufficient in all use cases".
///   - On Redox, only /dev/urandom is provided.
///   - On AIX, /dev/urandom will "provide cryptographically secure output".
///   - On Haiku and QNX Neutrino they are identical.
#[cfg(not(feature = "rustix"))]
const FILE_PATH: &str = "/dev/urandom\0";
#[cfg(feature = "rustix")]
const FILE_PATH: &str = "/dev/urandom";
const FD_UNINIT: usize = usize::max_value();

pub fn getrandom_inner(dest: &mut [MaybeUninit<u8>]) -> Result<(), Error> {
    let fd = get_rng_fd()?;
    sys_fill_exact(dest, |buf| read_from_fd(fd, buf))
}

#[cfg(not(feature = "rustix"))]
fn read_from_fd(fd: libc::c_int, buf: &mut [MaybeUninit<u8>]) -> libc::ssize_t {
    unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) }
}

#[cfg(feature = "rustix")]
fn read_from_fd(
    fd: libc::c_int,
    buf: &mut [MaybeUninit<u8>],
) -> Result<(&mut [u8], &mut [MaybeUninit<u8>]), rustix::io::Errno> {
    rustix::io::read_uninit(unsafe { rustix::fd::BorrowedFd::borrow_raw(fd) }, buf)
}

// Returns the file descriptor for the device file used to retrieve random
// bytes. The file will be opened exactly once. All subsequent calls will
// return the same file descriptor. This file descriptor is never closed.
fn get_rng_fd() -> Result<libc::c_int, Error> {
    static FD: AtomicUsize = AtomicUsize::new(FD_UNINIT);
    fn get_fd() -> Option<libc::c_int> {
        match FD.load(Relaxed) {
            FD_UNINIT => None,
            val => Some(val as libc::c_int),
        }
    }

    // Use double-checked locking to avoid acquiring the lock if possible.
    if let Some(fd) = get_fd() {
        return Ok(fd);
    }

    // SAFETY: We use the mutex only in this method, and we always unlock it
    // before returning, making sure we don't violate the pthread_mutex_t API.
    static MUTEX: Mutex = Mutex::new();
    unsafe { MUTEX.lock() };
    let _guard = DropGuard(|| unsafe { MUTEX.unlock() });

    if let Some(fd) = get_fd() {
        return Ok(fd);
    }

    // On Linux, /dev/urandom might return insecure values.
    #[cfg(any(target_os = "android", target_os = "linux"))]
    wait_until_rng_ready()?;

    #[allow(unused_unsafe)]
    let fd = unsafe { open_readonly(FILE_PATH)? };
    #[cfg(feature = "rustix")]
    let fd = rustix::fd::IntoRawFd::into_raw_fd(fd);
    // The fd always fits in a usize without conflicting with FD_UNINIT.
    debug_assert!(fd >= 0 && (fd as usize) < FD_UNINIT);
    FD.store(fd as usize, Relaxed);

    Ok(fd)
}

// Succeeds once /dev/urandom is safe to read from
#[cfg(all(
    any(target_os = "android", target_os = "linux"),
    not(feature = "rustix")
))]
fn wait_until_rng_ready() -> Result<(), Error> {
    // Poll /dev/random to make sure it is ok to read from /dev/urandom.
    let fd = unsafe { open_readonly("/dev/random\0")? };
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let _guard = DropGuard(|| unsafe {
        libc::close(fd);
    });

    loop {
        // A negative timeout means an infinite timeout.
        let res = unsafe { libc::poll(&mut pfd, 1, -1) };
        if res >= 0 {
            debug_assert_eq!(res, 1); // We only used one fd, and cannot timeout.
            return Ok(());
        }
        let err = crate::util_libc::last_os_error();
        match err.raw_os_error() {
            Some(libc::EINTR) | Some(libc::EAGAIN) => continue,
            _ => return Err(err),
        }
    }
}

// Succeeds once /dev/urandom is safe to read from
#[cfg(all(any(target_os = "android", target_os = "linux"), feature = "rustix"))]
fn wait_until_rng_ready() -> Result<(), Error> {
    use rustix::event;

    // Open the file.
    let fd = crate::util_rustix::open_readonly("/dev/random")?;

    // Poll it until it is ready.
    let mut pfd = [event::PollFd::new(&fd, event::PollFlags::IN)];
    loop {
        match event::poll(&mut pfd, -1) {
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(err) => return Err(crate::util_rustix::cvt(err)),
        }
    }
}

struct Mutex(UnsafeCell<libc::pthread_mutex_t>);

impl Mutex {
    const fn new() -> Self {
        Self(UnsafeCell::new(libc::PTHREAD_MUTEX_INITIALIZER))
    }
    unsafe fn lock(&self) {
        let r = libc::pthread_mutex_lock(self.0.get());
        debug_assert_eq!(r, 0);
    }
    unsafe fn unlock(&self) {
        let r = libc::pthread_mutex_unlock(self.0.get());
        debug_assert_eq!(r, 0);
    }
}

unsafe impl Sync for Mutex {}

struct DropGuard<F: FnMut()>(F);

impl<F: FnMut()> Drop for DropGuard<F> {
    fn drop(&mut self) {
        self.0()
    }
}
