//! Small Unix platform helpers kept outside display backends.

use std::error::Error;
use std::ffi::CStr;
use std::fmt;
use std::mem::MaybeUninit;
use std::ptr;

const INITIAL_PASSWD_BUFFER: usize = 16 * 1024;
const MAX_PASSWD_BUFFER: usize = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HardeningError {
    CoreLimit(i32),
    #[cfg(target_os = "linux")]
    Dumpable(i32),
}

impl fmt::Display for HardeningError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CoreLimit(code) => write!(f, "could not disable core dumps (errno {code})"),
            #[cfg(target_os = "linux")]
            Self::Dumpable(code) => {
                write!(f, "could not disable process dumpability (errno {code})")
            }
        }
    }
}

impl Error for HardeningError {}

/// Disable process dumps before any password input is accepted.
pub fn harden_process() -> Result<(), HardeningError> {
    let limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `limit` is a valid immutable `rlimit` for the duration of the
    // call and `RLIMIT_CORE` has no additional pointer requirements.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limit) } != 0 {
        return Err(HardeningError::CoreLimit(last_errno()));
    }

    #[cfg(target_os = "linux")]
    {
        // SAFETY: `PR_SET_DUMPABLE` takes one integer argument; remaining
        // variadic arguments are explicitly zeroed.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(HardeningError::Dumpable(last_errno()));
        }
    }
    Ok(())
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(-1)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UserError {
    NotFound,
    InvalidEncoding,
    BufferLimit,
    AllocationFailed,
    System(i32),
}

impl fmt::Display for UserError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("current user has no passwd entry"),
            Self::InvalidEncoding => f.write_str("current username is not valid UTF-8"),
            Self::BufferLimit => f.write_str("passwd entry exceeds the internal buffer limit"),
            Self::AllocationFailed => f.write_str("could not allocate passwd lookup buffer"),
            Self::System(code) => write!(f, "getpwuid_r failed with error {code}"),
        }
    }
}

impl Error for UserError {}

/// Resolve the real UID through the system passwd database. Environment
/// variables are intentionally not trusted for authentication identity.
pub fn current_username() -> Result<String, UserError> {
    // SAFETY: `getuid` has no arguments or preconditions.
    let uid = unsafe { libc::getuid() };
    let mut size = INITIAL_PASSWD_BUFFER;

    loop {
        let mut buffer = Vec::new();
        buffer
            .try_reserve_exact(size)
            .map_err(|_| UserError::AllocationFailed)?;
        buffer.resize(size, 0_u8);

        let mut entry = MaybeUninit::<libc::passwd>::zeroed();
        let mut result = ptr::null_mut();
        // SAFETY: `entry` points to writable storage, `buffer` is writable for
        // exactly `size` bytes, and `result` is a valid out-pointer. Pointers
        // returned inside `entry` remain valid while `buffer` is alive.
        let status = unsafe {
            libc::getpwuid_r(
                uid,
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                size,
                &mut result,
            )
        };

        if status == libc::ERANGE {
            size = size.checked_mul(2).ok_or(UserError::BufferLimit)?;
            if size > MAX_PASSWD_BUFFER {
                return Err(UserError::BufferLimit);
            }
            continue;
        }
        if status != 0 {
            return Err(UserError::System(status));
        }
        if result.is_null() {
            return Err(UserError::NotFound);
        }

        // SAFETY: successful `getpwuid_r` returns a valid NUL-terminated
        // `pw_name` pointer into `buffer` for the lifetime of this scope.
        let name = unsafe { CStr::from_ptr((*entry.as_ptr()).pw_name) };
        return name
            .to_str()
            .map(str::to_owned)
            .map_err(|_| UserError::InvalidEncoding);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_a_nonempty_current_username() {
        let username = current_username().unwrap();
        assert!(!username.is_empty());
        assert!(!username.as_bytes().contains(&0));
    }
}
