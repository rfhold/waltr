//! Descriptor-bound macOS extended-ACL inspection. No paths or credential data.
#![cfg(target_os = "macos")]
#![deny(unsafe_op_in_unsafe_fn)]

use std::{
    ffi::c_void,
    io,
    os::fd::{AsRawFd, BorrowedFd},
    ptr::NonNull,
};

/// ACL inspection failed or an extended ACL was present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

#[repr(C)]
struct Attributes {
    count: u16,
    reserved: u16,
    common: u32,
    volume: u32,
    directory: u32,
    file: u32,
    fork: u32,
}

unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, kind: u32) -> *mut c_void;
    fn acl_get_entry(acl: *mut c_void, id: i32, entry: *mut *mut c_void) -> i32;
    fn acl_valid(acl: *mut c_void) -> i32;
    fn acl_free(acl: *mut c_void) -> i32;
    fn fgetattrlist(
        fd: i32,
        attributes: *mut Attributes,
        buffer: *mut c_void,
        size: usize,
        options: u32,
    ) -> i32;
}

struct Acl(NonNull<c_void>);
impl Drop for Acl {
    fn drop(&mut self) {
        // SAFETY: this allocation came from acl_get_fd_np and has one owner.
        unsafe {
            acl_free(self.0.as_ptr());
        }
    }
}

/// Reject any extended ACL. The descriptor is borrowed and never closed.
pub fn ensure_no_extended_acl(fd: BorrowedFd<'_>) -> Result<(), Unavailable> {
    // SAFETY: fd remains live for this call; ACL_TYPE_EXTENDED is 0x100.
    let acl = unsafe { acl_get_fd_np(fd.as_raw_fd(), 0x100) };
    let Some(acl) = NonNull::new(acl) else {
        if io::Error::last_os_error().raw_os_error() != Some(2) {
            return Err(Unavailable);
        }
        // Darwin may report ENOENT for absent ACLs. Require independent,
        // successful descriptor metadata retrieval, not errno alone.
        return absent_security(fd);
    };
    let acl = Acl(acl);
    // SAFETY: acl owns a live native ACL allocation.
    if unsafe { acl_valid(acl.0.as_ptr()) } != 0 {
        return Err(Unavailable);
    }
    let mut entry = std::ptr::null_mut();
    // SAFETY: the validated ACL is live; entry is a writable out-pointer.
    // Darwin returns 0 for an entry and -1/EINVAL for an empty valid ACL.
    let result = unsafe { acl_get_entry(acl.0.as_ptr(), 0, &mut entry) };
    if result == -1 && io::Error::last_os_error().raw_os_error() == Some(22) && entry.is_null() {
        absent_security(fd)
    } else {
        Err(Unavailable)
    }
}

fn absent_security(fd: BorrowedFd<'_>) -> Result<(), Unavailable> {
    let mut attributes = Attributes {
        count: 5,
        reserved: 0,
        common: 0x00400000,
        volume: 0,
        directory: 0,
        file: 0,
        fork: 0,
    };
    let mut buffer = [0u32; 2048];
    // SAFETY: both buffers are initialized, aligned, writable and correctly
    // sized; ATTR_CMN_EXTENDED_SECURITY returns one attrreference_t.
    if unsafe {
        fgetattrlist(
            fd.as_raw_fd(),
            &mut attributes,
            buffer.as_mut_ptr().cast(),
            std::mem::size_of_val(&buffer),
            0,
        )
    } != 0
    {
        return Err(Unavailable);
    }
    let total = buffer[0] as usize;
    if !(12..=std::mem::size_of_val(&buffer)).contains(&total) {
        return Err(Unavailable);
    }
    let length = buffer[2] as usize;
    if length == 0 {
        return Ok(());
    }
    let offset = i32::from_ne_bytes(buffer[1].to_ne_bytes());
    let start = 4i64 + i64::from(offset);
    if start < 12 || start % 4 != 0 || length < 44 {
        return Err(Unavailable);
    }
    let start = start as usize;
    if start.checked_add(length).is_none_or(|end| end > total) {
        return Err(Unavailable);
    }
    if buffer[start / 4] == 0x012cc16d
        && matches!(buffer[start / 4 + 9], 0 | u32::MAX)
        && length == 44
    {
        Ok(())
    } else {
        Err(Unavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::{self, File},
        os::{fd::AsFd, unix::fs::PermissionsExt},
        path::PathBuf,
        process::Command,
        sync::atomic::{AtomicUsize, Ordering},
    };

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let path = std::env::temp_dir().join(format!(
                "waltr-acl-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(path)
        }
        fn grant(&self, path: &std::path::Path, entry: &str) {
            assert!(path.starts_with(&self.0));
            assert!(
                Command::new("/bin/chmod")
                    .arg("+a")
                    .arg(entry)
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn acl_free_files_and_directories_are_operable() {
        let fixture = Fixture::new();
        let dir = File::open(&fixture.0).unwrap();
        assert_eq!(ensure_no_extended_acl(dir.as_fd()), Ok(()));
        let file = File::create(fixture.0.join("record")).unwrap();
        assert_eq!(ensure_no_extended_acl(file.as_fd()), Ok(()));
        assert_eq!(absent_security(file.as_fd()), Ok(()));
    }

    #[test]
    fn inherited_allow_acl_is_rejected_before_writing() {
        let fixture = Fixture::new();
        fixture.grant(
            &fixture.0,
            "everyone allow read,write,execute,file_inherit,directory_inherit",
        );
        let dir = File::open(&fixture.0).unwrap();
        assert_eq!(ensure_no_extended_acl(dir.as_fd()), Err(Unavailable));
        let child = fixture.0.join("child");
        fs::create_dir(&child).unwrap();
        assert_eq!(
            ensure_no_extended_acl(File::open(child).unwrap().as_fd()),
            Err(Unavailable)
        );
        let file = File::create(fixture.0.join("record")).unwrap();
        assert_eq!(ensure_no_extended_acl(file.as_fd()), Err(Unavailable));
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn existing_record_acl_is_rejected_on_same_descriptor() {
        let fixture = Fixture::new();
        let path = fixture.0.join("record");
        let file = File::create(&path).unwrap();
        fixture.grant(&path, "everyone allow read");
        assert_eq!(ensure_no_extended_acl(file.as_fd()), Err(Unavailable));
        assert_eq!(absent_security(file.as_fd()), Err(Unavailable));
    }
}
