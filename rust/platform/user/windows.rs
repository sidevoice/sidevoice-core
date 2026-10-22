//! Windows: the user's SID, read from a process token with `windows-sys`.
//!
//! Own `unsafe` rather than a wrapper crate: the surface is five calls and the maintained crates around yield
//! names, not SIDs.

use std::io;
use std::ptr::null_mut;

use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{
    EqualSid, GetLengthSid, GetTokenInformation, TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, OpenProcess, OpenProcessToken, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Windows side of [`super::UserId`]: an owned copy of the user's SID. A SID is DWORD-aligned: the words keep that.
#[derive(Clone, Debug)]
pub(super) struct Id {
    words: Vec<u32>,
}

impl Id {
    fn as_psid(&self) -> PSID {
        self.words.as_ptr() as PSID
    }

    pub(super) fn sid_string(&self) -> io::Result<String> {
        let mut text: *mut u16 = null_mut();
        // SAFETY: the SID is a valid copy; on success `text` is a NUL-terminated string released below.
        if unsafe { ConvertSidToStringSidW(self.as_psid(), &mut text) } == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0;
        // SAFETY: the string is NUL-terminated, so reading up to the terminator stays inside it.
        let string = unsafe {
            while *text.add(len) != 0 {
                len += 1;
            }
            String::from_utf16_lossy(std::slice::from_raw_parts(text, len))
        };
        unsafe { LocalFree(text as _) };
        Ok(string)
    }
}

impl PartialEq for Id {
    fn eq(&self, other: &Self) -> bool {
        // SAFETY: both hold copies of valid SIDs.
        unsafe { EqualSid(self.as_psid(), other.as_psid()) != 0 }
    }
}

impl Eq for Id {}

pub(super) fn current() -> Id {
    // SAFETY: the pseudo-handle of the current process is valid and needs no closing.
    user_of_process(unsafe { GetCurrentProcess() }).expect("the process token is readable")
}

pub(super) fn of_process(pid: u32) -> io::Result<Id> {
    // SAFETY: a failure returns a null handle, checked; a success is closed below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if process.is_null() {
        return Err(io::Error::last_os_error());
    }
    let user = user_of_process(process);
    unsafe { CloseHandle(process) };
    user
}

fn user_of_process(process: HANDLE) -> io::Result<Id> {
    let mut token: HANDLE = null_mut();
    // SAFETY: `process` is a valid handle; the token is closed below.
    if unsafe { OpenProcessToken(process, TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let user = token_user(token);
    unsafe { CloseHandle(token) };
    user
}

fn token_user(token: HANDLE) -> io::Result<Id> {
    let mut needed = 0u32;
    // SAFETY: a null buffer of length 0 asks only for the size (it fails with INSUFFICIENT_BUFFER).
    unsafe { GetTokenInformation(token, TokenUser, null_mut(), 0, &mut needed) };
    if needed == 0 {
        return Err(io::Error::last_os_error());
    }
    // `u64` words: the struct holds a pointer, so the buffer must be pointer-aligned.
    let mut buffer = vec![0u64; (needed as usize).div_ceil(8)];
    // SAFETY: the buffer is at least `needed` bytes.
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenUser,
            buffer.as_mut_ptr() as *mut _,
            needed,
            &mut needed,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: success filled the buffer with a TOKEN_USER whose SID points inside it.
    let sid = unsafe { (*(buffer.as_ptr() as *const TOKEN_USER)).User.Sid };
    let len = unsafe { GetLengthSid(sid) } as usize;
    let mut words = vec![0u32; len.div_ceil(4)];
    // SAFETY: `sid` is valid for `len` bytes and `words` holds at least that many.
    unsafe { std::ptr::copy_nonoverlapping(sid as *const u8, words.as_mut_ptr() as *mut u8, len) };
    Ok(Id { words })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_has_a_sid_string() {
        let text = current().sid_string().unwrap();
        assert!(text.starts_with("S-1-"), "{text}");
    }

    #[test]
    fn this_process_is_this_user() {
        assert_eq!(of_process(std::process::id()).unwrap(), current());
    }

    #[test]
    fn local_system_is_another_user() {
        // S-1-5-18: revision 1, one sub-authority, NT authority, 18.
        let system = Id {
            words: vec![0x0000_0101, 0x0500_0000, 0x0000_0012],
        };
        assert_eq!(system.sid_string().unwrap(), "S-1-5-18");
        assert_ne!(current(), system);
    }
}
