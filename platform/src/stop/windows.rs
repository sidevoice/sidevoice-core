//! Windows: the named event `Local\sidevoice-stop-<pid>`. The process that listens creates it for itself
//! (`CreateEventW`, manual-reset, not set) and waits on it; whoever asks opens it by the pid (`OpenEventW`, the right
//! to set it and nothing else) and sets it (`SetEvent`). An event that does not exist is a process that does not
//! listen: nothing is asked. `Local\` is this logon session's namespace, where the user's processes and their
//! scheduled tasks (`InteractiveToken`) run.
//!
//! Who may set it: the same protected descriptor as the local channel's pipe ([`crate::ipc`]), its owner (the user
//! whose process made it), SYSTEM and Administrators, nobody else. SYSTEM and Administrators can end the process
//! anyway, so asking it to leave gives them nothing new, and one rule covers both ways into this user's processes.
//! The event's name is chosen by its pid, so it exists before the process only if one of this session's processes
//! (this user's) made it: that one is shared, as several listeners in one process share it.
//!
//! Waiting: a thread of its own blocks in `WaitForMultipleObjects` on the event and on a private, unnamed one that
//! dropping the listener sets, so the thread ends with the listener and never outlives it blocked (a `spawn_blocking`
//! task blocked for good would also hold up the `tokio` runtime's shutdown). Own `unsafe` over `windows-sys`: no
//! maintained crate covers named events, and the surface is these few calls.

use std::io;
use std::ptr::null;
use std::sync::Arc;
use tokio::sync::oneshot;
use widestring::U16CString;
use windows_sys::Win32::Foundation::{
    CloseHandle, LocalFree, ERROR_FILE_NOT_FOUND, FALSE, HANDLE, TRUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows_sys::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows_sys::Win32::System::Threading::{
    CreateEventW, OpenEventW, SetEvent, WaitForMultipleObjects, EVENT_MODIFY_STATE, INFINITE,
};

/// The listener: the events, and what the waiting thread sends once the request is set.
pub struct Os {
    events: Arc<Events>,
    asked: Option<oneshot::Receiver<()>>,
}

/// The request's event and the private one that ends the wait. Closed when the listener and its thread are done.
struct Events {
    request: Handle,
    done: Handle,
}

/// An open handle, closed on drop. Kernel handles may be used from any thread.
struct Handle(HANDLE);

// SAFETY: a kernel object handle is not tied to the thread that opened it.
unsafe impl Send for Handle {}
// SAFETY: as above; the calls made on it (wait, set) are thread-safe.
unsafe impl Sync for Handle {}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: the handle is open and owned here.
        unsafe { CloseHandle(self.0) };
    }
}

/// The name of `pid`'s event.
fn name(pid: u32) -> U16CString {
    U16CString::from_str(format!(r"Local\sidevoice-stop-{pid}")).expect("no NUL in the name")
}

/// A handle from a call that returns null on failure.
fn opened(handle: HANDLE) -> io::Result<Handle> {
    if handle.is_null() {
        Err(io::Error::last_os_error())
    } else {
        Ok(Handle(handle))
    }
}

/// This process's event, created with the owner-only descriptor (or opened, when this process already has one).
fn own_event() -> io::Result<Handle> {
    let sddl = U16CString::from_str(crate::ipc::OWNER_ONLY).map_err(io::Error::other)?;
    let mut descriptor: PSECURITY_DESCRIPTOR = std::ptr::null_mut();
    // SAFETY: a NUL-terminated SDDL string; on success `descriptor` is allocated by the call and freed below.
    let ok = unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor,
        bInheritHandle: FALSE,
    };
    // SAFETY: the attributes and the name live for the call; manual-reset, not set.
    let event = opened(unsafe {
        CreateEventW(&attributes, TRUE, FALSE, name(std::process::id()).as_ptr())
    });
    // SAFETY: allocated by the conversion above; the event keeps its own copy.
    unsafe { LocalFree(descriptor) };
    event
}

impl super::StopRequest for Os {
    fn listen() -> io::Result<Os> {
        let request = own_event()?;
        // SAFETY: an unnamed manual-reset event, not set, default security.
        let done = opened(unsafe { CreateEventW(null(), TRUE, FALSE, null()) })?;
        let events = Arc::new(Events { request, done });
        let (asked, waited) = oneshot::channel();
        let waiting = events.clone();
        std::thread::Builder::new()
            .name("sidevoice-stop-request".into())
            .spawn(move || {
                let handles = [waiting.request.0, waiting.done.0];
                // SAFETY: both handles stay open while `waiting` holds them.
                let woke = unsafe { WaitForMultipleObjects(2, handles.as_ptr(), FALSE, INFINITE) };
                if woke == WAIT_OBJECT_0 {
                    let _ = asked.send(());
                }
            })?;
        Ok(Os {
            events,
            asked: Some(waited),
        })
    }

    async fn received(&mut self) {
        if let Some(asked) = self.asked.as_mut() {
            if asked.await.is_err() {
                // The wait failed or was ended: no request will come.
                std::future::pending::<()>().await;
            }
            self.asked = None;
        }
    }

    fn ask(pid: u32) -> io::Result<bool> {
        super::checked(pid)?;
        // SAFETY: a NUL-terminated name; the handle is closed when `event` drops.
        let event =
            match opened(unsafe { OpenEventW(EVENT_MODIFY_STATE, FALSE, name(pid).as_ptr()) }) {
                Ok(event) => event,
                Err(error) if error.raw_os_error() == Some(ERROR_FILE_NOT_FOUND as i32) => {
                    return Ok(false)
                }
                Err(error) => return Err(error),
            };
        // SAFETY: the handle is open, with the right to set it.
        if unsafe { SetEvent(event.0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(true)
    }
}

impl Drop for Os {
    fn drop(&mut self) {
        // SAFETY: the handle is open while `self.events` holds it. The thread wakes, ends and lets go of the events.
        unsafe { SetEvent(self.events.done.0) };
    }
}
