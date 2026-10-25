//! The small Win32 seam for atomic private directory creation and inspection.

use std::ffi::c_void;
use std::io;
use std::os::windows::{
    ffi::OsStrExt,
    io::{FromRawHandle, OwnedHandle},
};
use std::path::Path;
use std::ptr::null_mut;

use windows_sys::Win32::{
    Foundation::LocalFree,
    Security::{
        ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            ConvertStringSidToSidW, GetNamedSecurityInfoW, SE_FILE_OBJECT,
        },
        DACL_SECURITY_INFORMATION, EqualSid, GetAce, GetSecurityDescriptorControl,
        GetTokenInformation, OWNER_SECURITY_INFORMATION, SE_DACL_PROTECTED, SECURITY_ATTRIBUTES,
        TOKEN_QUERY, TOKEN_USER, TokenUser,
    },
    Storage::FileSystem::{CreateDirectoryW, FILE_ALL_ACCESS, MOVEFILE_WRITE_THROUGH, MoveFileExW},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

// Both conversion and security-query APIs allocate on the Win32 local heap.
// Keeping that ownership explicit prevents leaks on every early error path.
struct Local(*mut c_void);
impl Drop for Local {
    fn drop(&mut self) {
        // SAFETY: only successful allocating Win32 calls construct Local.
        unsafe {
            LocalFree(self.0);
        }
    }
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    value.push(0);
    Ok(value)
}

fn user() -> io::Result<Vec<usize>> {
    let mut token = null_mut();
    // SAFETY: the process pseudo-handle is valid and token points to writable
    // output storage. OwnedHandle closes only the newly opened real handle.
    unsafe {
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = OwnedHandle::from_raw_handle(token);
        use std::os::windows::io::AsRawHandle;
        let mut size = 0;
        GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut size);
        if size == 0 {
            return Err(io::Error::last_os_error());
        }
        // usize storage provides TOKEN_USER's pointer alignment, unlike Vec<u8>.
        let mut bytes = vec![0_usize; (size as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.as_raw_handle(),
            TokenUser,
            bytes.as_mut_ptr().cast(),
            size,
            &mut size,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(bytes)
    }
}

fn descriptor() -> io::Result<Local> {
    let user = user()?;
    let mut sid = null_mut();
    // SAFETY: user retains the aligned TOKEN_USER buffer and embedded SID for
    // the duration of both conversion calls. Converted strings are terminated.
    unsafe {
        let user = &*user.as_ptr().cast::<TOKEN_USER>();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid = Local(sid.cast());
        let mut length = 0;
        while *sid.0.cast::<u16>().add(length) != 0 {
            length += 1;
        }
        let sid_text = String::from_utf16(std::slice::from_raw_parts(sid.0.cast(), length))
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
        // Protected DACL blocks inherited broad grants. OI/CI propagate the
        // same literal account SID to journal/lock files, not OWNER RIGHTS.
        let sddl: Vec<_> = format!("O:{sid_text}D:P(A;OICI;FA;;;{sid_text})")
            .encode_utf16()
            .chain([0])
            .collect();
        let mut descriptor = null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            1,
            &mut descriptor,
            null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Local(descriptor))
    }
}

// Elevated Windows processes may create files owned by Administrators
// or SYSTEM. Those principals already have OS-level takeover powers, like root
// on Unix; their ownership is not a grant to another ordinary user.
unsafe fn elevated_owner(owner: *mut c_void) -> io::Result<bool> {
    for sid in ["S-1-5-32-544", "S-1-5-18"] {
        let sid: Vec<_> = sid.encode_utf16().chain([0]).collect();
        let mut parsed = null_mut();
        // SAFETY: both SID strings are fixed, terminated and valid. The caller
        // supplies the owner pointer returned by GetNamedSecurityInfoW.
        unsafe {
            if ConvertStringSidToSidW(sid.as_ptr(), &mut parsed) == 0 {
                return Err(io::Error::last_os_error());
            }
            let parsed = Local(parsed);
            if EqualSid(owner, parsed.0) != 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(super) fn create_directory(path: &Path) -> io::Result<()> {
    let path = wide(path)?;
    let descriptor = descriptor()?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    };
    // SAFETY: the descriptor and terminated path outlive this synchronous call.
    if unsafe { CreateDirectoryW(path.as_ptr(), &attributes) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) fn check_private(path: &Path, directory: bool) -> io::Result<()> {
    let path = wide(path)?;
    let user = user()?;
    let mut owner = null_mut();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor = null_mut();
    // SAFETY: outputs point into descriptor's allocation and remain valid until
    // Local drops it. Win32 validates the descriptor/ACL returned from the file.
    unsafe {
        let result = GetNamedSecurityInfoW(
            path.as_ptr(),
            SE_FILE_OBJECT,
            OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
            &mut owner,
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        );
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
        let descriptor = Local(descriptor);
        let user = &*user.as_ptr().cast::<TOKEN_USER>();
        let mut control = 0;
        let mut revision = 0;
        if owner.is_null()
            || dacl.is_null()
            || (EqualSid(owner, user.User.Sid) == 0 && (directory || !elevated_owner(owner)?))
            || (*dacl).AceCount != 1
            || GetSecurityDescriptorControl(descriptor.0, &mut control, &mut revision) == 0
            || (directory && control & SE_DACL_PROTECTED == 0)
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let mut ace = null_mut();
        if GetAce(dacl, 0, &mut ace) == 0 {
            return Err(io::Error::last_os_error());
        }
        let header = &*ace.cast::<ACE_HEADER>();
        if header.AceType != 0 || usize::from(header.AceSize) < size_of::<ACCESS_ALLOWED_ACE>() {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        // Only a basic allow ACE applying to this object is accepted. Reject
        // additional principals, inherit-only grants and null/unrestricted ACLs.
        if ace.Header.AceType != 0
            || ace.Header.AceFlags & 0x08 != 0
            || ace.Mask & FILE_ALL_ACCESS != FILE_ALL_ACCESS
            || EqualSid(
                std::ptr::addr_of!(ace.SidStart).cast_mut().cast(),
                user.User.Sid,
            ) == 0
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
    }
    Ok(())
}

pub(super) fn publish_directory(staging: &Path, published: &Path) -> io::Result<()> {
    let staging = wide(staging)?;
    let published = wide(published)?;
    // SAFETY: both paths are terminated and live throughout the call. Omitting
    // REPLACE_EXISTING preserves the no-identity-replacement rule.
    if unsafe { MoveFileExW(staging.as_ptr(), published.as_ptr(), MOVEFILE_WRITE_THROUGH) } == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
