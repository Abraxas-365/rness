//! Windows local-only named pipe with an explicit current-user DACL.
use anyhow::{Context, bail};
use std::{path::Path, ptr};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use windows_sys::Win32::{
    Foundation::{CloseHandle, HANDLE, LocalFree},
    Security::Authorization::{
        ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
        SDDL_REVISION_1,
    },
    Security::{GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser},
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

pub fn validate(path: &Path) -> anyhow::Result<()> {
    let name = path.to_string_lossy();
    let suffix = name
        .strip_prefix(r"\\.\pipe\rness-")
        .context("expected local rness pipe name")?;
    if suffix.is_empty() || suffix.contains(['\\', '/']) {
        bail!(r"use a local pipe name: \\.\pipe\rness-<unique-name>");
    }
    Ok(())
}
pub fn server(path: &Path, first: bool) -> anyhow::Result<NamedPipeServer> {
    validate(path)?;
    // All raw allocations/handles are released on both success and error.
    unsafe {
        let mut token: HANDLE = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let result = (|| {
            let mut length = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
            let mut storage =
                vec![0usize; (length as usize + size_of::<usize>() - 1) / size_of::<usize>()];
            if GetTokenInformation(
                token,
                TokenUser,
                storage.as_mut_ptr().cast(),
                length,
                &mut length,
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let user = &*(storage.as_ptr().cast::<TOKEN_USER>());
            let mut sid = ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let mut count = 0;
            while *sid.add(count) != 0 {
                count += 1;
            }
            let sid_text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, count));
            LocalFree(sid.cast());
            let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{sid_text})\0")
                .encode_utf16()
                .collect();
            let mut security = ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                descriptor.as_ptr(),
                SDDL_REVISION_1,
                &mut security,
                ptr::null_mut(),
            ) == 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let attributes = SECURITY_ATTRIBUTES {
                nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
                lpSecurityDescriptor: security,
                bInheritHandle: 0,
            };
            let result = ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    path,
                    (&attributes as *const SECURITY_ATTRIBUTES)
                        .cast_mut()
                        .cast(),
                )
                .context("create private local control pipe");
            LocalFree(security);
            result
        })();
        CloseHandle(token);
        result
    }
}
