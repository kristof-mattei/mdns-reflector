use std::os::fd::AsRawFd as _;

use libc::{c_int, socklen_t};
use socket2::Socket;

/// Helper macro to execute a system call that returns an `io::Result`.
macro_rules! syscall {
    ($fn: ident ( $($arg: expr),* $(,)* ) ) => {{
        #[expect(clippy::allow_attributes, reason = "Not all calls mirrored are unsafe")]
        #[allow(unused_unsafe, reason = "Some calls are unsafe")]
        // SAFETY: libc call
        let res = unsafe { libc::$fn($($arg, )*) };
        if res == -1 {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(res)
        }
    }};
}

/// Caller must ensure `T` is the correct type for `opt` and `val`.
#[expect(
    clippy::needless_pass_by_value,
    reason = "We need a pointer TO the value"
)]
pub unsafe fn setsockopt<T>(
    fd: &Socket,
    opt: libc::c_int,
    val: c_int,
    payload: T,
) -> std::io::Result<()> {
    let payload = (&raw const payload).cast::<libc::c_void>();

    #[expect(
        clippy::cast_possible_truncation,
        reason = "It this doesn't fit we got bigger problems"
    )]
    let size = std::mem::size_of::<T>() as socklen_t;

    syscall!(setsockopt(fd.as_raw_fd(), opt, val, payload, size)).map(|_| ())
}
