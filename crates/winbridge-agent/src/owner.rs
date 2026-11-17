// SPDX-License-Identifier: GPL-3.0-or-later

//! Ownership of the files and directories the agent creates.
//!
//! The agent runs elevated, and an elevated token's default owner is the Administrators group, so
//! everything it created (staged drops, dropped files, clipboard staging) belonged to
//! `BUILTIN\Administrators` instead of the user. [`adopt_user_ownership`] makes the user the
//! default owner of the process token before anything is created. Files that are *moved* in
//! keep the owner and DACL they had, so [`claim`] fixes both after the move: the DACL is reset
//! to what the destination folder's inheritable entries give, and the owner becomes the user.

#[cfg(windows)]
use std::path::Path;

/// An empty ACL (revision 2, 8 bytes, no entries). With `UNPROTECTED_DACL_SECURITY_INFORMATION`
/// the system fills in the entries inherited from the parent folder.
#[cfg_attr(not(windows), allow(dead_code))]
fn empty_acl() -> [u8; 8] {
    let mut acl = [0u8; 8];
    acl[0] = 2; // ACL_REVISION
    acl[2..4].copy_from_slice(&8u16.to_le_bytes()); // AclSize
    acl
}

/// Whether the owner SID `current` differs from the user's `wanted`.
#[cfg_attr(not(windows), allow(dead_code))]
fn owner_differs(current: &[u8], wanted: &[u8]) -> bool {
    current != wanted
}

#[cfg(windows)]
mod imp {
    use super::{empty_acl, owner_differs, Path};
    use std::ptr;
    use std::sync::OnceLock;
    use tracing::{info, warn};
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetLengthSid, GetTokenInformation, SetTokenInformation, TokenOwner, TokenUser, ACL,
        DACL_SECURITY_INFORMATION, OWNER_SECURITY_INFORMATION, PSID, TOKEN_ADJUST_DEFAULT, TOKEN_OWNER,
        TOKEN_QUERY, TOKEN_USER, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    /// The user's SID (a copy of the bytes of the token's `TokenUser`).
    static USER_SID: OnceLock<Vec<u8>> = OnceLock::new();

    fn sid_bytes(sid: PSID) -> Vec<u8> {
        unsafe { std::slice::from_raw_parts(sid as *const u8, GetLengthSid(sid) as usize).to_vec() }
    }

    /// Makes the token's `TokenUser` the default owner of everything this process creates.
    ///
    /// As SYSTEM the process cannot take another user as its default owner. The threads that
    /// create the user's files impersonate the user instead ([`crate::identity`]), and [`claim`]
    /// and [`fix_owner`] correct what is moved in; this only records whose files they are.
    pub fn adopt_user_ownership() {
        if crate::identity::is_system() {
            match crate::identity::user_sid() {
                Some(sid) => {
                    let _ = USER_SID.set(sid.to_vec());
                    info!("New files and folders are created as the logged-on user");
                }
                None => info!("Nobody is logged on: there is no user to own files yet"),
            }
            return;
        }
        unsafe {
            let mut token: HANDLE = ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY | TOKEN_ADJUST_DEFAULT, &mut token) == 0 {
                warn!("Ownership: cannot open the process token: {}", std::io::Error::last_os_error());
                return;
            }
            let mut len = 0u32;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut len);
            let mut buf = vec![0u8; len as usize];
            if len == 0 || GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
                warn!("Ownership: cannot read the token user: {}", std::io::Error::last_os_error());
                CloseHandle(token);
                return;
            }
            let user = (*(buf.as_ptr() as *const TOKEN_USER)).User.Sid;
            let owner = TOKEN_OWNER { Owner: user };
            let ok = SetTokenInformation(
                token,
                TokenOwner,
                (&owner as *const TOKEN_OWNER).cast(),
                std::mem::size_of::<TOKEN_OWNER>() as u32,
            );
            if ok == 0 {
                warn!("Ownership: cannot set the default owner: {}", std::io::Error::last_os_error());
            } else {
                info!("New files and folders are now owned by the user instead of Administrators");
            }
            let _ = USER_SID.set(sid_bytes(user));
            CloseHandle(token);
        }
    }

    fn wide(path: &Path) -> Vec<u16> {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Owner SID of `path`.
    fn owner_of(path: &Path) -> Result<Vec<u8>, String> {
        let name = wide(path);
        unsafe {
            let mut owner: PSID = ptr::null_mut();
            let mut descriptor = ptr::null_mut();
            let err = GetNamedSecurityInfoW(
                name.as_ptr(),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                &mut owner,
                ptr::null_mut(),
                ptr::null_mut(),
                ptr::null_mut(),
                &mut descriptor,
            );
            if err != 0 {
                return Err(format!("GetNamedSecurityInfo error {}", err));
            }
            let bytes = sid_bytes(owner);
            LocalFree(descriptor);
            Ok(bytes)
        }
    }

    /// Makes the user the owner of `path` unless it already is (the DACL stays as it is).
    pub fn fix_owner(path: &Path) {
        let Some(user) = USER_SID.get() else {
            return;
        };
        match owner_of(path) {
            Ok(current) if !owner_differs(&current, user) => {}
            Ok(_) => {
                if let Err(e) = set_owner(path, user) {
                    warn!("Ownership: {:?} keeps its owner: {}", path, e);
                } else {
                    info!("Ownership: {:?} now belongs to the user", path);
                }
            }
            Err(e) => warn!("Ownership: cannot read the owner of {:?}: {}", path, e),
        }
    }

    fn set_owner(path: &Path, user: &[u8]) -> Result<(), String> {
        let name = wide(path);
        let err = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION,
                user.as_ptr() as PSID,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
            )
        };
        if err == 0 { Ok(()) } else { Err(format!("SetNamedSecurityInfo(owner) error {}", err)) }
    }

    /// For a file that was moved to `path`: takes the destination folder's inheritable entries
    /// (a rename keeps the old DACL) and makes the user the owner.
    pub fn claim(path: &Path) -> Result<(), String> {
        let name = wide(path);
        let acl = empty_acl();
        let err = unsafe {
            SetNamedSecurityInfoW(
                name.as_ptr() as *mut u16,
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl.as_ptr() as *const ACL,
                ptr::null(),
            )
        };
        if err != 0 {
            return Err(format!("SetNamedSecurityInfo(dacl) error {}", err));
        }
        if let Some(user) = USER_SID.get() {
            set_owner(path, user)?;
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::process::Command;

        fn icacls(path: &Path) -> String {
            let out = Command::new("icacls").arg(path).output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        /// Prints what it observes (`--nocapture`); `WB_KEEP_DIR` keeps the files for inspection.
        #[test]
        fn new_and_moved_files_belong_to_the_user_and_inherit_from_their_folder() {
            adopt_user_ownership();
            let user = USER_SID.get().expect("token user");
            let root = std::env::var_os("WB_KEEP_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join(format!("wb-owner-{}", std::process::id())));
            let _ = std::fs::remove_dir_all(&root);
            let from = root.join("from");
            let to = root.join("to");
            std::fs::create_dir_all(&from).unwrap();
            std::fs::create_dir_all(&to).unwrap();

            // A file this process creates is owned by the user (the token's default owner).
            let created = from.join("created.txt");
            std::fs::write(&created, b"x").unwrap();
            assert!(!owner_differs(&owner_of(&created).unwrap(), user), "created file keeps Administrators as owner");
            let made = to.join("dir");
            std::fs::create_dir(&made).unwrap();
            assert!(!owner_differs(&owner_of(&made).unwrap(), user), "created folder keeps Administrators as owner");

            // A source folder whose ACL differs from the destination's: only Everyone.
            let status = Command::new("icacls")
                .arg(&from)
                .args(["/inheritance:r", "/grant", "*S-1-1-0:(OI)(CI)F"])
                .output()
                .unwrap();
            assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stdout));
            let staged = from.join("staged.txt");
            std::fs::write(&staged, b"payload").unwrap();
            let dest = to.join("moved.txt");
            std::fs::rename(&staged, &dest).unwrap();
            let before = icacls(&dest);
            println!("--- moved file, before claim ---\n{}", before);
            assert!(before.contains("Everyone"), "the rename should have kept the old ACL");

            claim(&dest).unwrap();
            let after = icacls(&dest);
            println!("--- moved file, after claim ---\n{}", after);
            assert!(!after.contains("Everyone"), "the destination's ACL was not applied");
            assert!(after.contains("(I)"), "entries are inherited from the destination folder");
            assert!(!owner_differs(&owner_of(&dest).unwrap(), user));
            assert_eq!(std::fs::read(&dest).unwrap(), b"payload");

            if std::env::var_os("WB_KEEP_DIR").is_none() {
                let _ = Command::new("icacls").arg(&root).args(["/grant", "*S-1-1-0:(OI)(CI)F", "/T", "/C", "/Q"]).output();
                let _ = std::fs::remove_dir_all(&root);
            }
        }
    }
}

#[cfg(windows)]
pub use imp::{adopt_user_ownership, claim, fix_owner};

#[cfg(not(windows))]
mod imp {
    use std::path::Path;

    pub fn adopt_user_ownership() {}

    pub fn fix_owner(_path: &Path) {}

    pub fn claim(_path: &Path) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(not(windows))]
pub use imp::{adopt_user_ownership, claim, fix_owner};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_empty_acl_is_a_valid_header_without_entries() {
        let acl = empty_acl();
        assert_eq!(acl[0], 2, "revision");
        assert_eq!(u16::from_le_bytes([acl[2], acl[3]]), 8, "size covers exactly the header");
        assert_eq!(u16::from_le_bytes([acl[4], acl[5]]), 0, "no entries");
    }

    #[test]
    fn only_a_different_sid_needs_a_fix() {
        let user = [1u8, 1, 0, 0, 0, 0, 0, 5, 21, 0, 0, 0, 7];
        let admins = [1u8, 2, 0, 0, 0, 0, 0, 5, 32, 0, 0, 0, 32, 2, 0, 0];
        assert!(!owner_differs(&user, &user));
        assert!(owner_differs(&admins, &user));
    }
}
