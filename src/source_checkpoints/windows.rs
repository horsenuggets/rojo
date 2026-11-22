//! Private creation, followed by validation on the very handle used for content I/O.
//!
//! Namespace handles deny delete sharing while a Store is alive. This does not pin
//! arbitrary project ancestors or promise protection against privileged ACL changes.

use std::{
    ffi::c_void,
    fs::File,
    io,
    mem::{size_of, zeroed},
    os::windows::{
        ffi::OsStrExt,
        io::{AsRawHandle, FromRawHandle, OwnedHandle},
    },
    path::Path,
    ptr::{null, null_mut},
};

use windows_sys::Win32::{
    Foundation::{
        GetLastError, LocalFree, ERROR_ALREADY_EXISTS, ERROR_INSUFFICIENT_BUFFER, GENERIC_READ,
        GENERIC_WRITE, INVALID_HANDLE_VALUE,
    },
    Security::{
        Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            GetSecurityInfo, SDDL_REVISION_1, SE_FILE_OBJECT,
        },
        EqualSid, GetAce, GetLengthSid, GetSecurityDescriptorControl, GetTokenInformation,
        IsValidAcl, IsValidSid, TokenUser, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL,
        CONTAINER_INHERIT_ACE, DACL_SECURITY_INFORMATION, OBJECT_INHERIT_ACE,
        OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
        SE_DACL_PROTECTED, TOKEN_QUERY, TOKEN_USER,
    },
    Storage::FileSystem::{
        CreateDirectoryW, CreateFileW, GetFileInformationByHandle, GetVolumeInformationByHandleW,
        BY_HANDLE_FILE_INFORMATION, CREATE_NEW, FILE_ALL_ACCESS, FILE_ATTRIBUTE_DIRECTORY,
        FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
        FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        OPEN_EXISTING, READ_CONTROL,
    },
    System::Threading::{GetCurrentProcess, OpenProcessToken},
};

// FILE_PERSISTENT_ACLS from GetVolumeInformationByHandleW's documented flags.
const FILE_PERSISTENT_ACLS: u32 = 0x00000008;
const ACCESS_ALLOWED_ACE_TYPE: u8 = 0;
const ACL_REVISION: u8 = 2;
const ACL_REVISION_DS: u8 = 4;

struct LocalAllocation(*mut c_void);

impl Drop for LocalAllocation {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: These allocations are returned by the documented LocalAlloc APIs.
            unsafe {
                LocalFree(self.0);
            }
        }
    }
}

fn checked(success: i32) -> io::Result<()> {
    if success == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}

fn wide(path: &Path) -> io::Result<Vec<u16>> {
    let mut value: Vec<_> = path.as_os_str().encode_wide().collect();
    if value.contains(&0) {
        return Err(invalid("Checkpoint path contains a null character"));
    }
    value.push(0);
    Ok(value)
}

struct CurrentUser {
    // usize alignment is needed for TOKEN_USER, which contains a pointer.
    buffer: Vec<usize>,
}

impl CurrentUser {
    fn new() -> io::Result<Self> {
        unsafe {
            let mut token = null_mut();
            checked(OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_QUERY,
                &mut token,
            ))?;
            let token = OwnedHandle::from_raw_handle(token);
            let mut length = 0;
            GetTokenInformation(token.as_raw_handle(), TokenUser, null_mut(), 0, &mut length);
            if GetLastError() != ERROR_INSUFFICIENT_BUFFER
                || length < size_of::<TOKEN_USER>() as u32
            {
                return Err(io::Error::last_os_error());
            }
            let mut buffer = vec![0usize; (length as usize).div_ceil(size_of::<usize>())];
            checked(GetTokenInformation(
                token.as_raw_handle(),
                TokenUser,
                buffer.as_mut_ptr().cast(),
                length,
                &mut length,
            ))?;
            let user = Self { buffer };
            if IsValidSid(user.sid()) == 0 {
                return Err(invalid("Invalid process TokenUser SID"));
            }
            Ok(user)
        }
    }

    fn sid(&self) -> PSID {
        // SAFETY: The buffer holds the successful TokenUser result for its full lifetime.
        unsafe { (*(self.buffer.as_ptr().cast::<TOKEN_USER>())).User.Sid }
    }

    fn descriptor(&self, directory: bool) -> io::Result<LocalAllocation> {
        unsafe {
            let mut text = null_mut();
            checked(ConvertSidToStringSidW(self.sid(), &mut text))?;
            let _text = LocalAllocation(text.cast());
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            let sid = String::from_utf16(std::slice::from_raw_parts(text, length))
                .map_err(|_| invalid("Invalid process SID string"))?;
            let flags = if directory { "OICI" } else { "" };
            let sddl: Vec<u16> = format!("O:{sid}D:P(A;{flags};FA;;;{sid})")
                .encode_utf16()
                .chain([0])
                .collect();
            descriptor_from_sddl(&sddl)
        }
    }
}

fn descriptor_from_sddl(sddl: &[u16]) -> io::Result<LocalAllocation> {
    if sddl.last() != Some(&0) {
        return Err(invalid("SDDL must be null terminated"));
    }
    let mut descriptor = null_mut();
    // SAFETY: All callers supply a null-terminated SDDL buffer.
    checked(unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            null_mut(),
        )
    })?;
    Ok(LocalAllocation(descriptor))
}

fn attributes(descriptor: &LocalAllocation) -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: 0,
    }
}

fn native_open(
    path: &Path,
    writable: bool,
    create: bool,
    directory: bool,
    temporary: bool,
    security: Option<&SECURITY_ATTRIBUTES>,
) -> io::Result<File> {
    let path = wide(path)?;
    let access = if directory {
        READ_CONTROL
    } else {
        GENERIC_READ | if writable { GENERIC_WRITE } else { 0 }
    };
    let share = FILE_SHARE_READ | FILE_SHARE_WRITE | if temporary { FILE_SHARE_DELETE } else { 0 };
    let flags = FILE_FLAG_OPEN_REPARSE_POINT
        | if directory {
            FILE_FLAG_BACKUP_SEMANTICS
        } else {
            FILE_ATTRIBUTE_NORMAL
        };
    // SAFETY: Path and optional security descriptor remain live throughout CreateFileW.
    let handle = unsafe {
        CreateFileW(
            path.as_ptr(),
            access,
            share,
            security.map_or(null(), |value| value as *const _),
            if create { CREATE_NEW } else { OPEN_EXISTING },
            flags,
            null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: CreateFileW transferred ownership of a valid handle.
    Ok(unsafe { File::from_raw_handle(handle) })
}

fn validate(file: &File, user: &CurrentUser, directory: bool, privacy: bool) -> io::Result<()> {
    unsafe {
        let handle = file.as_raw_handle();
        let mut info: BY_HANDLE_FILE_INFORMATION = zeroed();
        checked(GetFileInformationByHandle(handle, &mut info))?;
        if info.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || (info.dwFileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0) != directory
            || (!directory && info.nNumberOfLinks != 1)
        {
            return Err(invalid("Checkpoint storage refuses reparse points, unexpected types, and additional hard links"));
        }
        let mut flags = 0;
        checked(GetVolumeInformationByHandleW(
            handle,
            null_mut(),
            0,
            null_mut(),
            null_mut(),
            &mut flags,
            null_mut(),
            0,
        ))?;
        if flags & FILE_PERSISTENT_ACLS == 0 {
            return Err(invalid(
                "Checkpoint storage requires a volume with persistent Windows ACLs",
            ));
        }
        if !privacy {
            return Ok(());
        }
        let mut owner: PSID = null_mut();
        let mut dacl: *mut ACL = null_mut();
        let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
        let result = GetSecurityInfo(
            handle,
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
        let _descriptor = LocalAllocation(descriptor);
        if descriptor.is_null() {
            return Err(invalid("Checkpoint security descriptor is unavailable"));
        }
        let mut control = 0;
        let mut revision = 0;
        checked(GetSecurityDescriptorControl(
            descriptor,
            &mut control,
            &mut revision,
        ))?;
        if owner.is_null()
            || IsValidSid(owner) == 0
            || EqualSid(owner, user.sid()) == 0
            || control & SE_DACL_PROTECTED == 0
            || dacl.is_null()
            || IsValidAcl(dacl) == 0
        {
            return Err(invalid("Checkpoint storage requires the process user as owner and a protected nonnull private DACL. Existing caches are never silently repaired"));
        }
        if !matches!((*dacl).AclRevision, ACL_REVISION | ACL_REVISION_DS) || (*dacl).AceCount != 1 {
            return Err(invalid(
                "Checkpoint storage has an unsupported or nonprivate ACL",
            ));
        }
        let mut ace = null_mut();
        checked(GetAce(dacl, 0, &mut ace))?;
        let header = &*ace.cast::<ACE_HEADER>();
        let expected_flags = if directory {
            OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE
        } else {
            0
        };
        if header.AceType != ACCESS_ALLOWED_ACE_TYPE
            || u32::from(header.AceFlags) != expected_flags
            || (header.AceSize as usize) < std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart) + 8
        {
            return Err(invalid(
                "Checkpoint storage has an unsupported access entry",
            ));
        }
        let ace = &*ace.cast::<ACCESS_ALLOWED_ACE>();
        if ace.Mask != FILE_ALL_ACCESS {
            return Err(invalid(
                "Checkpoint storage has an unsupported access entry",
            ));
        }
        let sid = (&ace.SidStart as *const u32).cast_mut().cast();
        if IsValidSid(sid) == 0
            || GetLengthSid(sid) as usize + std::mem::offset_of!(ACCESS_ALLOWED_ACE, SidStart)
                > header.AceSize as usize
            || EqualSid(sid, user.sid()) == 0
        {
            return Err(invalid(
                "Checkpoint storage grants access to a different principal",
            ));
        }
        Ok(())
    }
}

pub(super) fn private_directory(path: &Path) -> anyhow::Result<File> {
    let user = CurrentUser::new()?;
    let descriptor = user.descriptor(true)?;
    let attributes = attributes(&descriptor);
    let name = wide(path)?;
    // No inherited permissive interval exists, even before the first content byte.
    if unsafe { CreateDirectoryW(name.as_ptr(), &attributes) } == 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_ALREADY_EXISTS as i32) {
            return Err(error.into());
        }
    }
    let file = native_open(path, false, false, true, false, None)?;
    // .rojo is shared with unrelated caches. Do not modify or impose a new ACL on it.
    validate(
        &file,
        &user,
        true,
        path.file_name().is_none_or(|name| name != ".rojo"),
    )?;
    Ok(file)
}

pub(super) fn create_file(path: &Path, temporary: bool) -> io::Result<File> {
    let user = CurrentUser::new()?;
    let descriptor = user.descriptor(false)?;
    let file = native_open(
        path,
        true,
        true,
        false,
        temporary,
        Some(&attributes(&descriptor)),
    )?;
    validate(&file, &user, false, true)?;
    Ok(file)
}

pub(super) fn open_file(path: &Path, writable: bool) -> io::Result<File> {
    let user = CurrentUser::new()?;
    let file = native_open(path, writable, false, false, false, None)?;
    validate(&file, &user, false, true)?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::super::{
        tests::{project, record},
        Store, MAX_TOTAL_BYTES,
    };
    use super::*;
    use std::{
        fs,
        io::{Read, Write},
    };
    use windows_sys::Win32::Security::{
        Authorization::SetNamedSecurityInfoW, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    fn user_sid() -> String {
        let user = CurrentUser::new().unwrap();
        unsafe {
            let mut text = null_mut();
            checked(ConvertSidToStringSidW(user.sid(), &mut text)).unwrap();
            let _text = LocalAllocation(text.cast());
            let mut length = 0;
            while *text.add(length) != 0 {
                length += 1;
            }
            String::from_utf16(std::slice::from_raw_parts(text, length)).unwrap()
        }
    }

    fn descriptor(sddl: &str) -> LocalAllocation {
        descriptor_from_sddl(&sddl.encode_utf16().chain([0]).collect::<Vec<_>>()).unwrap()
    }

    fn security_bytes(file: &File) -> Vec<u8> {
        unsafe {
            let mut security = null_mut();
            assert_eq!(
                GetSecurityInfo(
                    file.as_raw_handle(),
                    SE_FILE_OBJECT,
                    OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    null_mut(),
                    &mut security
                ),
                0
            );
            let _security = LocalAllocation(security);
            let length = windows_sys::Win32::Security::GetSecurityDescriptorLength(security);
            std::slice::from_raw_parts(security.cast::<u8>(), length as usize).to_vec()
        }
    }

    #[test]
    fn permissive_parent_is_unchanged_and_every_sensitive_file_is_private_before_content() {
        let (directory, project) = project();
        let cache = directory.path().join(".rojo");
        let sid = user_sid();
        let security = descriptor(&format!("O:{sid}D:P(A;OICI;FA;;;WD)"));
        checked(unsafe {
            CreateDirectoryW(wide(&cache).unwrap().as_ptr(), &attributes(&security))
        })
        .unwrap();
        let parent = native_open(&cache, false, false, true, false, None).unwrap();
        let before = security_bytes(&parent);
        let store = Store::open(&project).unwrap();
        assert_eq!(security_bytes(&parent), before);
        assert!(fs::rename(&store.directory, cache.join("replaced")).is_err());
        assert!(fs::rename(
            cache.join("source-checkpoints"),
            cache.join("replaced-cache")
        )
        .is_err());
        for file in [
            &store._lock,
            &open_file(&store.directory.join("owner.json"), false).unwrap(),
        ] {
            validate(file, &CurrentUser::new().unwrap(), false, true).unwrap();
        }
        let mut temporary = store.temporary_file().unwrap();
        assert_eq!(temporary.as_file().metadata().unwrap().len(), 0);
        validate(
            temporary.as_file(),
            &CurrentUser::new().unwrap(),
            false,
            true,
        )
        .unwrap();
        temporary.write_all(b"private editor content").unwrap();
        let created = store.create(record(&store)).unwrap();
        validate(
            &open_file(&store.record_path(&created).unwrap(), false).unwrap(),
            &CurrentUser::new().unwrap(),
            false,
            true,
        )
        .unwrap();
    }

    #[test]
    fn insecure_existing_lock_and_owner_fail_without_repairing_or_pruning() {
        for name in [".lock", "owner.json"] {
            let (_directory, project) = project();
            let store = Store::open(&project).unwrap();
            let identity = store.identity.clone();
            let id = store.create(record(&store)).unwrap();
            let record_path = store.record_path(&id).unwrap();
            let original = fs::read(&record_path).unwrap();
            let path = store.directory.join(name);
            drop(store);
            let content = fs::read(&path).unwrap();
            fs::remove_file(&path).unwrap();
            let sid = user_sid();
            let security = descriptor(&format!("O:{sid}D:(A;;FA;;;{sid})"));
            let mut file = native_open(
                &path,
                true,
                true,
                false,
                false,
                Some(&attributes(&security)),
            )
            .unwrap();
            file.write_all(&content).unwrap();
            let before = security_bytes(&file);
            assert!(Store::open_identity(&identity).is_err());
            assert_eq!(security_bytes(&file), before);
            assert_eq!(fs::read(record_path).unwrap(), original);
        }
    }

    #[test]
    fn rejects_null_unprotected_permissive_and_unsupported_acls_without_repair() {
        let directory = tempfile::tempdir().unwrap();
        let sid = user_sid();
        for (index, (sddl, unprotect)) in [
            (format!("O:{sid}D:PNO_ACCESS_CONTROL"), false),
            (format!("O:{sid}D:P(A;;FA;;;{sid})"), true),
            (format!("O:{sid}D:P(A;;FA;;;WD)"), false),
            (format!("O:{sid}D:P(A;;FR;;;{sid})"), false),
            (format!("O:{sid}D:P(A;;FA;;;{sid})(A;;FR;;;WD)"), false),
        ]
        .into_iter()
        .enumerate()
        {
            let path = directory.path().join(format!("insecure-{index}"));
            let security = descriptor(&sddl);
            let mut file = native_open(
                &path,
                true,
                true,
                false,
                false,
                Some(&attributes(&security)),
            )
            .unwrap();
            file.write_all(b"existing content").unwrap();
            if unprotect {
                // Windows may protect an explicit creation DACL. Clear protection on the
                // owned fixture afterward so this case exercises an actually unsafe ACL.
                // SAFETY: Handles, paths, and API-allocated descriptors remain valid
                // throughout each call.
                unsafe {
                    let mut dacl = null_mut();
                    let mut security = null_mut();
                    assert_eq!(
                        GetSecurityInfo(
                            file.as_raw_handle(),
                            SE_FILE_OBJECT,
                            DACL_SECURITY_INFORMATION,
                            null_mut(),
                            null_mut(),
                            &mut dacl,
                            null_mut(),
                            &mut security,
                        ),
                        0
                    );
                    let _security = LocalAllocation(security);
                    let mut name = wide(&path).unwrap();
                    assert_eq!(
                        SetNamedSecurityInfoW(
                            name.as_mut_ptr(),
                            SE_FILE_OBJECT,
                            DACL_SECURITY_INFORMATION | UNPROTECTED_DACL_SECURITY_INFORMATION,
                            null_mut(),
                            null_mut(),
                            dacl,
                            null_mut(),
                        ),
                        0
                    );
                    let mut updated = null_mut();
                    assert_eq!(
                        GetSecurityInfo(
                            file.as_raw_handle(),
                            SE_FILE_OBJECT,
                            DACL_SECURITY_INFORMATION,
                            null_mut(),
                            null_mut(),
                            null_mut(),
                            null_mut(),
                            &mut updated,
                        ),
                        0
                    );
                    let _updated = LocalAllocation(updated);
                    let mut control = 0;
                    let mut revision = 0;
                    checked(GetSecurityDescriptorControl(
                        updated,
                        &mut control,
                        &mut revision,
                    ))
                    .unwrap();
                    assert_eq!(control & SE_DACL_PROTECTED, 0);
                }
            }
            let before = security_bytes(&file);
            assert!(
                validate(&file, &CurrentUser::new().unwrap(), false, true).is_err(),
                "{sddl}"
            );
            assert!(open_file(&path, false).is_err(), "{sddl}");
            assert_eq!(security_bytes(&file), before);
            assert_eq!(file.metadata().unwrap().len(), 16);
        }
    }

    #[test]
    fn wrong_owner_and_creation_security_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("wrong-owner");
        let sid = user_sid();
        let security = descriptor(&format!("O:WDD:P(A;;FA;;;{sid})"));
        // Normal users cannot assign World as owner. If creation is permitted by the
        // test account, handle validation must still reject it before content I/O.
        if let Ok(file) = native_open(
            &path,
            true,
            true,
            false,
            false,
            Some(&attributes(&security)),
        ) {
            assert!(validate(&file, &CurrentUser::new().unwrap(), false, true).is_err());
            assert_eq!(file.metadata().unwrap().len(), 0);
        }
        assert!(create_file(&directory.path().join("missing/record"), false).is_err());
        assert!(descriptor_from_sddl(&[b'!' as u16, 0]).is_err());
    }

    #[test]
    fn rejects_hard_links_reparse_points_and_namespace_collisions() {
        let (directory, project) = project();
        let store = Store::open(&project).unwrap();
        let id = store.create(record(&store)).unwrap();
        let path = store.record_path(&id).unwrap();
        let link = directory.path().join("linked");
        fs::hard_link(&path, &link).unwrap();
        assert!(store.read(&id).is_err());
        assert!(store.discard(&id).is_err());
        assert!(store.create(record(&store)).is_err());
        fs::remove_file(link).unwrap();
        assert_eq!(store.read(&id).unwrap().source, "editor source\n");
        let collision = store.directory.join("collision");
        let mut original = create_file(&collision, false).unwrap();
        original.write_all(b"original").unwrap();
        assert!(create_file(&collision, false).is_err());
        let mut recovered = String::new();
        open_file(&collision, false)
            .unwrap()
            .read_to_string(&mut recovered)
            .unwrap();
        assert_eq!(recovered, "original");
        drop(original);
        assert!(private_directory(&collision).is_err());
        let symbolic = store.directory.join("symbolic");
        match std::os::windows::fs::symlink_file(&path, &symbolic) {
            Ok(()) => assert!(open_file(&symbolic, false).is_err()),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                eprintln!("Reparse fixture requires Windows symlink privilege.")
            }
            Err(error) => panic!("Cannot create reparse fixture. {error}"),
        }
    }

    #[test]
    fn insecure_restart_and_incomplete_collision_never_prune_old_records() {
        let (_directory, project) = project();
        let store = Store::open(&project).unwrap();
        let identity = store.identity.clone();
        let id = store.create(record(&store)).unwrap();
        let old_path = store.record_path(&id).unwrap();
        let old = fs::read(&old_path).unwrap();
        let temp = store.temporary_file().unwrap();
        temp.as_file().set_len(MAX_TOTAL_BYTES).unwrap();
        let (file, abandoned) = temp.keep().unwrap();
        drop(file);
        // A reserved-name hard link is not an identified safe incomplete write.
        let alias = store.directory.join("alias");
        fs::hard_link(&abandoned, &alias).unwrap();
        drop(store);
        assert!(Store::open_identity(&identity).is_err());
        assert!(abandoned.exists());
        assert_eq!(fs::read(&old_path).unwrap(), old);
        fs::remove_file(alias).unwrap();
        let store = Store::open_identity(&identity).unwrap();
        assert!(!abandoned.exists());
        assert_eq!(store.read(&id).unwrap().source, "editor source\n");
        store.create(record(&store)).unwrap();
    }
}
