//! Process trees: a test's program and every process it starts stop together, at its timeout, at Ctrl-C and when it
//! exits (a job object on Windows, a process group on Unix). Anvil also puts itself in a job, so that whoever kills
//! Anvil (an IDE's stop button) stops its compilers and tests with it.

use std::io;
use std::process::{Child, Command, ExitStatus};

/// A started program and the processes it starts.
pub struct ProcessTree {
    child: Child,
    #[cfg(windows)]
    job: Option<windows::Job>,
}

impl ProcessTree {
    /// Starts the command in a tree of its own.
    #[cfg(windows)]
    pub fn spawn(command: &mut Command) -> io::Result<ProcessTree> {
        use std::os::windows::process::CommandExt;
        use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

        // Suspended until it is in the job, so nothing it starts can slip out
        command.creation_flags(CREATE_SUSPENDED);
        let mut child = command.spawn()?;
        let job = windows::Job::create().filter(|job| job.assign(&child));

        if let Err(error) = windows::resume(child.id()) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(error);
        }

        Ok(ProcessTree { child, job })
    }

    #[cfg(unix)]
    pub fn spawn(command: &mut Command) -> io::Result<ProcessTree> {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
        Ok(ProcessTree { child: command.spawn()? })
    }

    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.child.try_wait()
    }

    /// Stops every process of the tree still running and waits for the program.
    pub fn kill(&mut self) {
        #[cfg(windows)]
        match &self.job {
            Some(job) => job.terminate(),
            None => {
                let _ = self.child.kill();
            }
        }

        // Its process group, whose id is the program's
        #[cfg(unix)]
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }

        let _ = self.child.wait();
    }
}

/// Puts Anvil in a job that stops everything it started when Anvil dies or is killed. `release` before a normal exit.
#[cfg(windows)]
pub fn contain_self() {
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let Some(job) = windows::Job::create() else { return };
    if unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job.handle, GetCurrentProcess()) } != 0 {
        let _ = windows::OWN_JOB.set(job);
    }
}

#[cfg(not(windows))]
pub fn contain_self() {}

/// On a normal exit, what Anvil started and should outlive it (the compiler's PDB server) goes on running.
#[cfg(windows)]
pub fn release() {
    if let Some(job) = windows::OWN_JOB.get() {
        job.set_limits(windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_BREAKAWAY_OK);
    }
}

#[cfg(not(windows))]
pub fn release() {}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Thread32First, Thread32Next, TH32CS_SNAPTHREAD, THREADENTRY32,
    };
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation, SetInformationJobObject, TerminateJobObject,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };
    use windows_sys::Win32::System::Threading::{OpenThread, ResumeThread, THREAD_SUSPEND_RESUME};

    /// Anvil's own job, kept open for the life of the process.
    pub static OWN_JOB: std::sync::OnceLock<Job> = std::sync::OnceLock::new();

    /// A job object whose processes die when its last handle closes. Processes that ask to leave it may.
    pub struct Job {
        pub handle: HANDLE,
    }

    // A job handle may be used from any thread
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn create() -> Option<Job> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }

            let job = Job { handle };
            job.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK)
                .then_some(job)
        }

        /// Puts a process in the job; false when Windows refuses (a job that allows no nesting).
        pub fn assign(&self, child: &Child) -> bool {
            unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as HANDLE) != 0 }
        }

        pub fn terminate(&self) {
            unsafe { TerminateJobObject(self.handle, 1) };
        }

        pub fn set_limits(&self, limits: JOB_OBJECT_LIMIT) -> bool {
            let mut information: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            information.BasicLimitInformation.LimitFlags = limits;

            let succeeded = unsafe {
                SetInformationJobObject(
                    self.handle,
                    JobObjectExtendedLimitInformation,
                    &information as *const _ as *const std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            succeeded != 0
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            unsafe { CloseHandle(self.handle) };
        }
    }

    /// Resumes the threads of a process started suspended (only its first thread exists yet).
    pub fn resume(process_id: u32) -> io::Result<()> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<THREADENTRY32>() as u32;
        let mut resumed = 0;
        let mut more = unsafe { Thread32First(snapshot, &mut entry) } != 0;
        while more {
            if entry.th32OwnerProcessID == process_id {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if !thread.is_null() {
                    if unsafe { ResumeThread(thread) } != u32::MAX {
                        resumed += 1;
                    }
                    unsafe { CloseHandle(thread) };
                }
            }
            more = unsafe { Thread32Next(snapshot, &mut entry) } != 0;
        }
        unsafe { CloseHandle(snapshot) };

        if resumed == 0 {
            return Err(io::Error::other("could not resume its first thread"));
        }
        Ok(())
    }
}
