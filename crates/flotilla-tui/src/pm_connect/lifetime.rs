//! The process boundary is kept outside the connector's host-side tests.

#[cfg(unix)]
pub struct Parent(libc::pid_t);

#[cfg(unix)]
impl Parent {
    pub fn capture() -> Result<Self, String> {
        // SAFETY: getppid takes no arguments and has no memory preconditions.
        Ok(Self(unsafe { libc::getppid() }))
    }

    pub fn alive(&self) -> bool {
        // SAFETY: getppid takes no arguments and has no memory preconditions.
        unsafe { libc::getppid() == self.0 }
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_TIMEOUT},
        System::{
            Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS},
            Threading::{GetCurrentProcessId, OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE},
        },
    };

    pub struct Parent(HANDLE);

    impl Parent {
        pub fn capture() -> Result<Self, String> {
            // SAFETY: initialized ToolHelp record with its required size; all
            // owned snapshot/process handles are closed on every return path.
            unsafe {
                let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
                if snapshot == INVALID_HANDLE_VALUE {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                let mut entry: PROCESSENTRY32W = std::mem::zeroed();
                entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
                let mut found = Process32FirstW(snapshot, &mut entry);
                let mut parent = 0;
                while found != 0 {
                    if entry.th32ProcessID == GetCurrentProcessId() {
                        parent = entry.th32ParentProcessID;
                        break;
                    }
                    found = Process32NextW(snapshot, &mut entry);
                }
                CloseHandle(snapshot);
                if parent == 0 {
                    return Err("could not identify presentation manager parent".to_owned());
                }
                let handle = OpenProcess(PROCESS_SYNCHRONIZE, 0, parent);
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error().to_string());
                }
                Ok(Self(handle))
            }
        }

        pub fn alive(&self) -> bool {
            // SAFETY: this owner retains a valid synchronization handle.
            unsafe { WaitForSingleObject(self.0, 0) == WAIT_TIMEOUT }
        }
    }

    impl Drop for Parent {
        fn drop(&mut self) {
            // SAFETY: this owner uniquely owns the process handle.
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[cfg(windows)]
pub use windows::Parent;
