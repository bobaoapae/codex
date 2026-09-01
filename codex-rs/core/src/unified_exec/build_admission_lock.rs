//! Platform-specific nonblocking file-lock primitives for build admission.

use std::fs::File;

#[cfg(unix)]
pub(super) fn try_lock_file(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor belongs to the open lock file and remains valid
    // for the duration of this call.
    let result = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if result == 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if matches!(
            error.raw_os_error(),
            Some(code) if code == libc::EWOULDBLOCK || code == libc::EAGAIN
        ) {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            Err(error)
        }
    }
}

#[cfg(unix)]
pub(super) fn process_is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: kill with signal zero only probes process liveness.
    let result = unsafe { libc::kill(pid, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(unix)]
pub(super) fn unlock_file(file: &File) {
    use std::os::fd::AsRawFd;
    // SAFETY: the descriptor belongs to this guard's lock file.
    let _ = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(windows)]
pub(super) fn try_lock_file(file: &File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_FAIL_IMMEDIATELY;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: OVERLAPPED is a C POD structure whose zero value is the
    // documented synchronous-operation initializer.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is valid and the overlapped structure lives through
    // the synchronous nonblocking call.
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as _,
            LOCKFILE_EXCLUSIVE_LOCK | LOCKFILE_FAIL_IMMEDIATELY,
            0,
            1,
            0,
            &mut overlapped,
        )
    };
    if result != 0 {
        Ok(())
    } else {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(33) {
            Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
        } else {
            Err(error)
        }
    }
}

#[cfg(windows)]
pub(super) fn unlock_file(file: &File) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;
    // SAFETY: OVERLAPPED is a C POD structure whose zero value is the
    // documented synchronous-operation initializer.
    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    // SAFETY: the handle is valid and this guard owns the region lock.
    let _ = unsafe { UnlockFileEx(file.as_raw_handle() as _, 0, 1, 0, &mut overlapped) };
}

#[cfg(windows)]
pub(super) fn process_is_alive(pid: u32) -> bool {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::Foundation::STILL_ACTIVE;
    use windows_sys::Win32::System::Threading::GetExitCodeProcess;
    use windows_sys::Win32::System::Threading::OpenProcess;
    use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;
    use windows_sys::Win32::System::Threading::PROCESS_SYNCHRONIZE;
    // SAFETY: OpenProcess receives a validated PID and read-only rights.
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
            0,
            pid,
        )
    };
    if handle == 0 {
        return false;
    }
    let mut exit_code = 0_u32;
    // SAFETY: handle is valid until CloseHandle below; output pointer is valid.
    let active = unsafe {
        GetExitCodeProcess(handle, &mut exit_code) != 0 && exit_code == STILL_ACTIVE as u32
    };
    // SAFETY: handle was returned by OpenProcess above.
    unsafe { CloseHandle(handle) };
    active
}

#[cfg(not(any(unix, windows)))]
pub(super) fn try_lock_file(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(not(any(unix, windows)))]
pub(super) fn unlock_file(_file: &File) {}

#[cfg(not(any(unix, windows)))]
pub(super) fn process_is_alive(_pid: u32) -> bool {
    false
}
