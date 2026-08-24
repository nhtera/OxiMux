//! Directory junctions — the Windows equivalent of the atomic swap.
//!
//! # Why junctions carry the install
//!
//! Windows refuses to overwrite a running `.exe`. An installer that copies over
//! a live driver therefore fails exactly when it matters most — during an
//! upgrade — so the executable is never overwritten at all: each release lands
//! in its own immutable directory and a junction is *retargeted* to point at
//! it. That is one kernel call, the link is never absent mid-swap, and nothing
//! cares what is currently running. It is the same guarantee `renamex_np` gives
//! the macOS recipe, reached by a different mechanism.
//!
//! # Junctions, not symlinks
//!
//! Directory junctions are reparse points (`IO_REPARSE_TAG_MOUNT_POINT`) that
//! any unprivileged user can create. Directory *symlinks* need elevation or
//! Developer Mode. That difference is the whole reason this install needs no
//! UAC prompt, and it is why upstream's installer chose junctions too — the
//! layout here is deliberately theirs, so the two installers can coexist.
//!
//! # Reading needs none of this
//!
//! `fs::canonicalize` follows a junction, so resolving one is safe std code.
//! Only *creating* one needs the ioctl below.

use std::ffi::OsStr;
use std::io;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::MetadataExt;
use std::path::Path;

use windows_sys::Win32::Foundation::{CloseHandle, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows_sys::Win32::System::Ioctl::FSCTL_SET_REPARSE_POINT;
use windows_sys::Win32::System::IO::DeviceIoControl;

/// `IO_REPARSE_TAG_MOUNT_POINT`.
const TAG_MOUNT_POINT: u32 = 0xA000_0003;

/// Header before the mount-point payload: tag (4) + data length (2) +
/// reserved (2).
const HEADER_BYTES: usize = 8;
/// The four `u16` offsets/lengths that open the mount-point payload.
const MOUNT_POINT_FIELDS_BYTES: usize = 8;

/// Is `path` a reparse point (junction or symlink) rather than a real
/// directory?
///
/// The distinction is load-bearing: a *real* directory at a link path belongs
/// to someone else — quite possibly a previous upstream install — and must
/// never be replaced. Upstream's installer refuses the same way, and clobbering
/// it would permanently break the user's ability to run their installer.
pub(super) fn is_reparse_point(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .is_ok_and(|meta| meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0)
}

/// Point the junction at `link` to `target`, creating the link if needed.
///
/// Setting the reparse point on an existing junction retargets it in place —
/// no delete-then-create, so no instant where the path is missing.
pub(super) fn set_target(link: &Path, target: &Path) -> io::Result<()> {
    if !link.exists() && !is_reparse_point(link) {
        std::fs::create_dir_all(link)?;
    } else if !is_reparse_point(link) && link.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} is a real directory, not a junction — refusing to replace it",
                link.display()
            ),
        ));
    }

    let buffer = mount_point_buffer(target)?;
    let handle = open_reparse_handle(link)?;
    let mut returned: u32 = 0;
    // SAFETY: `handle` is a live directory handle opened with
    // FILE_FLAG_OPEN_REPARSE_POINT, and `buffer` is a correctly sized
    // REPARSE_DATA_BUFFER whose declared length matches its allocation.
    let ok = unsafe {
        DeviceIoControl(
            handle,
            FSCTL_SET_REPARSE_POINT,
            buffer.as_ptr().cast(),
            buffer.len() as u32,
            std::ptr::null_mut(),
            0,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    // SAFETY: `handle` came from CreateFileW and is closed exactly once.
    unsafe { CloseHandle(handle) };

    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn open_reparse_handle(link: &Path) -> io::Result<windows_sys::Win32::Foundation::HANDLE> {
    let wide = wide(link.as_os_str());
    // SAFETY: `wide` is NUL-terminated and outlives the call.
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    Ok(handle)
}

/// Build the `REPARSE_DATA_BUFFER` for a mount point.
///
/// The substitute name is an NT-namespace path (`\??\C:\...`); the print name
/// is the plain one Explorer shows. Both are NUL-terminated inside the buffer,
/// and neither NUL is counted in its declared length — the layout every
/// junction implementation agrees on.
///
/// The path is made absolute *lexically* — deliberately not with
/// `fs::canonicalize`, which resolves reparse points. Resolving would silently
/// collapse a junction-to-a-junction into a junction straight to the final
/// directory, which is exactly the link this install needs to keep: the visible
/// `bin` must follow `current` wherever it later points, not pin itself to
/// whichever release `current` happened to name at install time.
fn mount_point_buffer(target: &Path) -> io::Result<Vec<u8>> {
    let absolute = std::path::absolute(target)?;
    let display = absolute.to_string_lossy();
    // A path that already carries the `\\?\` extended-length prefix spells the
    // same thing the NT namespace writes as `\??\`.
    let plain = display.strip_prefix(r"\\?\").unwrap_or(&display);
    let substitute: Vec<u16> = OsStr::new(&format!(r"\??\{plain}"))
        .encode_wide()
        .collect();
    let print: Vec<u16> = OsStr::new(plain).encode_wide().collect();

    let substitute_bytes = substitute.len() * 2;
    let print_bytes = print.len() * 2;
    // Substitute name, its NUL, print name, its NUL.
    let path_bytes = substitute_bytes + 2 + print_bytes + 2;
    let data_length = MOUNT_POINT_FIELDS_BYTES + path_bytes;

    let mut buffer = Vec::with_capacity(HEADER_BYTES + data_length);
    buffer.extend_from_slice(&TAG_MOUNT_POINT.to_le_bytes());
    buffer.extend_from_slice(&(data_length as u16).to_le_bytes());
    buffer.extend_from_slice(&0u16.to_le_bytes()); // Reserved
    buffer.extend_from_slice(&0u16.to_le_bytes()); // SubstituteNameOffset
    buffer.extend_from_slice(&(substitute_bytes as u16).to_le_bytes());
    buffer.extend_from_slice(&((substitute_bytes + 2) as u16).to_le_bytes()); // PrintNameOffset
    buffer.extend_from_slice(&(print_bytes as u16).to_le_bytes());
    for unit in substitute {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer.extend_from_slice(&0u16.to_le_bytes());
    for unit in print {
        buffer.extend_from_slice(&unit.to_le_bytes());
    }
    buffer.extend_from_slice(&0u16.to_le_bytes());
    Ok(buffer)
}

fn wide(text: &OsStr) -> Vec<u16> {
    text.encode_wide().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_junction_resolves_to_its_target_and_retargets_in_place() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = root.path().join("release-1");
        let second = root.path().join("release-2");
        std::fs::create_dir(&first).expect("mkdir");
        std::fs::create_dir(&second).expect("mkdir");
        std::fs::write(first.join("marker"), b"one").expect("write");
        std::fs::write(second.join("marker"), b"two").expect("write");

        let link = root.path().join("current");
        set_target(&link, &first).expect("create junction");
        assert!(is_reparse_point(&link));
        assert_eq!(
            std::fs::read(link.join("marker")).expect("read through junction"),
            b"one"
        );

        // The upgrade: one call, and the link is never absent.
        set_target(&link, &second).expect("retarget junction");
        assert_eq!(
            std::fs::read(link.join("marker")).expect("read through junction"),
            b"two"
        );
    }

    /// A junction whose target is itself a junction must stay pointed at the
    /// *link*, not at whatever that link currently resolves to. The install's
    /// two-junction chain depends on it: retargeting the inner one has to move
    /// the outer one with it.
    #[test]
    fn a_junction_to_a_junction_follows_the_link_not_its_target() {
        let root = tempfile::tempdir().expect("tempdir");
        let first = root.path().join("release-1");
        let second = root.path().join("release-2");
        std::fs::create_dir(&first).expect("mkdir");
        std::fs::create_dir(&second).expect("mkdir");
        std::fs::write(first.join("marker"), b"one").expect("write");
        std::fs::write(second.join("marker"), b"two").expect("write");

        let inner = root.path().join("current");
        let outer = root.path().join("bin");
        set_target(&inner, &first).expect("inner junction");
        set_target(&outer, &inner).expect("outer junction");
        assert_eq!(std::fs::read(outer.join("marker")).expect("read"), b"one");

        // One retarget of the inner link; the outer must follow.
        set_target(&inner, &second).expect("retarget inner");
        assert_eq!(
            std::fs::read(outer.join("marker")).expect("read"),
            b"two",
            "the outer junction pinned itself to a release instead of following"
        );
    }

    #[test]
    fn a_real_directory_is_never_replaced() {
        let root = tempfile::tempdir().expect("tempdir");
        let target = root.path().join("release");
        std::fs::create_dir(&target).expect("mkdir");
        let occupied = root.path().join("bin");
        std::fs::create_dir(&occupied).expect("mkdir");
        std::fs::write(occupied.join("someone-elses-file"), b"").expect("write");

        let err = set_target(&occupied, &target).expect_err("must refuse");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            occupied.join("someone-elses-file").exists(),
            "the existing directory must be left exactly as it was"
        );
    }
}
