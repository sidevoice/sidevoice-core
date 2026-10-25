//! Held ancestry excludes rename/delete. Descendants are opened no-follow and deleted by handle, including reparse
//! entries. Own bindings cover disposition and the normalized name of an already checked handle: std exposes
//! neither. Existing fs owns identity, owner and ACL checks.
use crate::fs::{Files, Fs, HeldState, Others};
use crate::user::{User, UserId};
use std::ffi::{c_void, OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::windows::ffi::OsStringExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows_sys::Win32::Foundation::{
    ERROR_INVALID_FUNCTION, ERROR_INVALID_PARAMETER, ERROR_NOT_SUPPORTED, GENERIC_READ,
};
use windows_sys::Win32::Storage::FileSystem::{
    FileDispositionInfo, FileDispositionInfoEx, GetFinalPathNameByHandleW,
    SetFileInformationByHandle, DELETE, FILE_ATTRIBUTE_REPARSE_POINT, FILE_DISPOSITION_FLAG_DELETE,
    FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE, FILE_DISPOSITION_FLAG_POSIX_SEMANTICS,
    FILE_DISPOSITION_INFO, FILE_DISPOSITION_INFO_EX, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_NAME_NORMALIZED, FILE_READ_ATTRIBUTES, FILE_SHARE_READ,
    FILE_SHARE_WRITE, READ_CONTROL, VOLUME_NAME_NONE,
};

pub(super) fn contents(root: &HeldState, allowed: &[&OsStr], remove: bool) -> io::Result<()> {
    for name in allowed {
        crate::fs::record_name(name)?;
    }
    root.verify()?;
    for name in allowed {
        visit(root, &root.leaf().0.join(name), remove, 0)?;
    }
    root.verify()
}

fn visit(root: &HeldState, path: &Path, remove: bool, depth: usize) -> io::Result<()> {
    if depth > 64 {
        return Err(super::refused("removal tree is too deep"));
    }
    root.verify()?;
    // Read access participates in sharing checks; metadata-only handles would not block replacement. Every
    // ordinary ancestor remains open on the recursion stack without delete sharing until its children are done.
    let file = match OpenOptions::new()
        .read(true)
        .access_mode(
            GENERIC_READ | READ_CONTROL | FILE_READ_ATTRIBUTES | if remove { DELETE } else { 0 },
        )
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if depth == 0 {
        // Syntactic validation cannot distinguish a literal name from an NTFS 8.3 alias. Check the opened,
        // unrenameable entry's normalized long name, both during whole-list preflight and again at deletion.
        require_exact_name(&file, path.file_name().expect("validated child name"))?;
    }
    let meta = file.metadata()?;
    if Files::identity(&file)?.volume != Files::identity(root.leaf().1)?.volume {
        return Err(super::refused("removal crosses a volume boundary"));
    }
    if meta.is_dir() && meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT == 0 {
        if !UserId::current().owns_file(&file)?
            || matches!(Files::others_of(&file)?, Others::Change)
        {
            return Err(super::refused(
                "unsafe removal directory owner or permissions",
            ));
        }
        for entry in fs::read_dir(path)? {
            visit(root, &entry?.path(), remove, depth + 1)?;
        }
    }
    if remove {
        root.verify()?;
        delete(&file)?;
    }
    Ok(())
}

fn require_exact_name(file: &File, allowed: &OsStr) -> io::Result<()> {
    let mut name = vec![0u16; 512];
    loop {
        // SAFETY: file is held open without delete sharing; name is writable for the number of WCHARs passed.
        // NORMALIZED expands 8.3 names. NONE avoids requiring a DOS drive letter for this volume.
        let length = unsafe {
            GetFinalPathNameByHandleW(
                file.as_raw_handle(),
                name.as_mut_ptr(),
                name.len() as u32,
                FILE_NAME_NORMALIZED | VOLUME_NAME_NONE,
            )
        } as usize;
        if length == 0 {
            return Err(io::Error::last_os_error());
        }
        if length >= name.len() {
            if length > 32768 {
                return Err(super::refused(
                    "removal entry name exceeds the Windows path limit",
                ));
            }
            name.resize(length + 1, 0);
            continue;
        }
        let actual = OsString::from_wide(&name[..length]);
        if Path::new(&actual).file_name() != Some(allowed) {
            return Err(super::refused(
                "removal allowlist must use the exact on-disk name, not an alias",
            ));
        }
        return Ok(());
    }
}

fn delete(file: &File) -> io::Result<()> {
    let flags = FILE_DISPOSITION_INFO_EX {
        Flags: FILE_DISPOSITION_FLAG_DELETE
            | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS
            | FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE,
    };
    // SAFETY: file owns a live DELETE handle, and the buffer and class agree for the duration of the call.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfoEx,
            &flags as *const _ as *const c_void,
            std::mem::size_of_val(&flags) as u32,
        )
    } != 0
    {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if !matches!(
        error.raw_os_error().map(|code| code as u32),
        Some(ERROR_INVALID_PARAMETER | ERROR_NOT_SUPPORTED | ERROR_INVALID_FUNCTION)
    ) {
        return Err(error);
    }
    // Older filesystems can still unlink ordinary files; never clear READONLY to make fallback succeed, since
    // doing that also changes an external hard link. Windows may keep the name delete-pending until handles close.
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: same live handle and a correctly sized FILE_DISPOSITION_INFO buffer.
    if unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle(),
            FileDispositionInfo,
            &disposition as *const _ as *const c_void,
            std::mem::size_of_val(&disposition) as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
