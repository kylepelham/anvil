//! Process trees: a test's program and every process it starts stop together at its timeout and at Ctrl-C (a job
//! object on Windows, a process group on Unix); what a test leaves running when it ends by itself (a server later
//! tests use) goes on. Anvil also puts itself in a job, so that whoever kills Anvil (an IDE's stop button) stops its
//! compilers and tests with it (on Linux, `anvil run`'s program and the tests end with Anvil). On Unix, SIGTERM and
//! SIGHUP stop the running tests' process groups before Anvil ends, and reach `anvil run`'s program; after a SIGKILL,
//! which nothing can catch, only the programs Anvil started directly end with it (Linux), not what they started.

use std::io;
use std::process::{Child, Command, ExitStatus};
use std::sync::atomic::AtomicBool;
#[cfg(unix)]
use std::sync::atomic::{AtomicI32, Ordering};

/// `anvil run`'s program is running: Ctrl-C is its to handle (it gets the console's Ctrl-C itself), Anvil waits.
pub static FOREGROUND_PROGRAM: AtomicBool = AtomicBool::new(false);

// How many running tests' process groups a SIGTERM to Anvil stops (more run only with as many jobs; those beyond end
// with Anvil on Linux, without their children)
#[cfg(unix)]
const GROUP_SLOTS: usize = 256;

// The process groups of the running tests (0: a free slot), read by the signal handler
#[cfg(unix)]
static GROUPS: [AtomicI32; GROUP_SLOTS] = [const { AtomicI32::new(0) }; GROUP_SLOTS];

// `anvil run`'s program, as kill() takes it: its process id when it shares Anvil's process group (a terminal's), minus
// its group's id when it has its own; 0 when none runs
#[cfg(unix)]
static RUN_PROGRAM: AtomicI32 = AtomicI32::new(0);

// cl's PDB server (/Zi, vcpkg's port builds) is shared by every compiler using the same endpoint name
#[cfg(windows)]
const PDB_SERVER_VARIABLE: &str = "_MSPDBSRV_ENDPOINT_";

/// A started program and the processes it starts.
pub struct ProcessTree {
    child: Child,
    #[cfg(windows)]
    job: Option<windows::Job>,
    /// Its slot in GROUPS, when it got one.
    #[cfg(unix)]
    slot: Option<usize>,
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

        if let Err(error) = windows::resume(&child) {
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
        end_with_anvil(command);
        let child = command.spawn()?;

        // Its group, whose id is the program's, for a SIGTERM to Anvil to stop
        let group = child.id() as i32;
        let slot = GROUPS
            .iter()
            .position(|slot| slot.compare_exchange(0, group, Ordering::SeqCst, Ordering::SeqCst).is_ok());

        Ok(ProcessTree { child, slot })
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

    /// After the program ended: are processes it started still running?
    pub fn has_survivors(&self) -> bool {
        #[cfg(windows)]
        return self.job.as_ref().is_some_and(|job| job.active_processes() > 0);

        // Signal 0 only asks whether the process group still has a member
        #[cfg(unix)]
        return unsafe { libc::kill(-(self.child.id() as i32), 0) } == 0;
    }
}

#[cfg(unix)]
impl Drop for ProcessTree {
    fn drop(&mut self) {
        if let Some(slot) = self.slot {
            GROUPS[slot].store(0, Ordering::SeqCst);
        }
    }
}

/// Makes a program Anvil starts and waits for (`anvil run`'s, a test's) end when Anvil does, however Anvil ends (an
/// IDE's stop button, a CI timeout), as Anvil's job does on Windows: on Linux the kernel kills it when the thread that
/// started it ends. Not done for compiles: they end soon by themselves, and spawning without it is faster.
#[cfg(target_os = "linux")]
pub fn end_with_anvil(command: &mut Command) {
    use std::os::unix::process::CommandExt;

    let anvil = std::process::id() as libc::pid_t;
    unsafe {
        command.pre_exec(move || {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);

            // Anvil ended before that: nothing would signal it
            if libc::getppid() != anvil {
                libc::raise(libc::SIGKILL);
            }
            Ok(())
        });
    }
}

#[cfg(not(target_os = "linux"))]
pub fn end_with_anvil(_command: &mut Command) {}

/// Runs `anvil run`'s program and waits for it. With a terminal it shares Anvil's process group, so the terminal's
/// Ctrl-C, Ctrl-Z and input reach it as they would a program the shell started; without one (an IDE, CI, a script) it
/// gets a group of its own, which a SIGTERM, SIGHUP or SIGINT to Anvil reaches whole, its children too.
#[cfg(unix)]
pub fn run_program(command: &mut Command) -> io::Result<ExitStatus> {
    use std::os::unix::process::CommandExt;

    let terminal = [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO]
        .iter()
        .any(|descriptor| unsafe { libc::isatty(*descriptor) } == 1);
    if !terminal {
        command.process_group(0);
    }

    let mut child = command.spawn()?;
    let id = child.id() as i32;
    RUN_PROGRAM.store(if terminal { id } else { -id }, Ordering::SeqCst);
    let status = child.wait();
    RUN_PROGRAM.store(0, Ordering::SeqCst);

    status
}

#[cfg(windows)]
pub fn run_program(command: &mut Command) -> io::Result<ExitStatus> {
    command.status()
}

/// Ctrl-C while `anvil run`'s program runs: passed on when it has a process group of its own (no terminal sends it
/// there); one sharing Anvil's gets the terminal's own.
pub fn forward_interrupt() {
    #[cfg(unix)]
    {
        let program = RUN_PROGRAM.load(Ordering::SeqCst);
        if program < 0 {
            unsafe { libc::kill(program, libc::SIGINT) };
        }
    }
}

/// SIGTERM and SIGHUP: `anvil run`'s program gets the signal and Anvil waits for it, exiting as it does; otherwise the
/// running tests' process groups are killed and Anvil ends by the signal. Only async-signal-safe calls here.
#[cfg(unix)]
extern "C" fn on_termination(signal: libc::c_int) {
    let program = RUN_PROGRAM.load(Ordering::SeqCst);
    if program != 0 {
        unsafe { libc::kill(program, signal) };
        return;
    }

    for slot in &GROUPS {
        let group = slot.load(Ordering::SeqCst);
        if group > 0 {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
    }

    // Ended by the signal itself, as the shell expects (exit code 128 + the signal); delivered once the handler returns
    unsafe {
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// Puts Anvil in a job that stops everything it started when Anvil dies or is killed. `release` before a normal exit.
#[cfg(windows)]
pub fn contain_self() {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, JOB_OBJECT_LIMIT_BREAKAWAY_OK, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let Some(job) = windows::Job::create() else { return };
    if !job.set_limits(JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE | JOB_OBJECT_LIMIT_BREAKAWAY_OK) {
        return;
    }
    if unsafe { AssignProcessToJobObject(job.handle, GetCurrentProcess()) } != 0 {
        let _ = windows::OWN_JOB.set(job);
    }
}

/// Stops what Anvil started when it is terminated (SIGTERM, SIGHUP): see `on_termination`.
#[cfg(unix)]
pub fn contain_self() {
    let handler: extern "C" fn(libc::c_int) = on_termination;
    for signal in [libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = handler as libc::sighandler_t;
            action.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(signal, &action, std::ptr::null_mut());
        }
    }
}

/// Keeps Anvil's own input and output handles out of the processes it starts, except as the standard handles it
/// gives them: a test's server outliving Anvil would otherwise hold Anvil's output open, and whoever reads it (a CI
/// log, a pipe) would wait for the server.
#[cfg(windows)]
pub fn keep_handles_to_self() {
    use windows_sys::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};

    for which in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        let handle = unsafe { GetStdHandle(which) };
        if !handle.is_null() && handle != INVALID_HANDLE_VALUE {
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT, 0) };
        }
    }
}

#[cfg(not(windows))]
pub fn keep_handles_to_self() {}

/// Gives the compilers Anvil starts a PDB server of this run's own, unless one is named already: a killed Anvil
/// takes its PDB server with it, which must not be one other builds use.
#[cfg(windows)]
pub fn separate_pdb_server() {
    if std::env::var_os(PDB_SERVER_VARIABLE).is_none() {
        std::env::set_var(PDB_SERVER_VARIABLE, format!("anvil-{}", std::process::id()));
    }
}

#[cfg(not(windows))]
pub fn separate_pdb_server() {}

/// On a normal exit, what Anvil started and should outlive it (a compiler cache's server, a test's) goes on running.
#[cfg(windows)]
pub fn release() {
    if let Some(job) = windows::OWN_JOB.get() {
        job.set_limits(windows_sys::Win32::System::JobObjects::JOB_OBJECT_LIMIT_BREAKAWAY_OK);
    }
}

#[cfg(not(windows))]
pub fn release() {}

/// Ends Anvil with this exit code, its output flushed. On Windows the process terminates itself instead of calling
/// ExitProcess, which kills the other threads wherever they are (a rayon pool's workers freeing memory as they end, the
/// language server's compiler detection starting vcvarsall.bat) and then runs the DLLs' detach code in this thread:
/// an allocation there can spin forever on heap state a killed thread left half-changed. Nothing Anvil needs runs at
/// detach.
#[cfg(windows)]
pub fn exit(code: i32) -> ! {
    use std::io::Write;
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};

    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    unsafe { TerminateProcess(GetCurrentProcess(), code as u32) };

    // Not reached: terminating itself does not return
    std::process::exit(code)
}

#[cfg(not(windows))]
pub fn exit(code: i32) -> ! {
    std::process::exit(code)
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicAccountingInformation, JobObjectExtendedLimitInformation,
        QueryInformationJobObject, SetInformationJobObject, TerminateJobObject, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
        JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT,
    };

    // Resumes every thread of a process; there is no Win32 call for it
    #[link(name = "ntdll", kind = "raw-dylib")]
    extern "system" {
        fn NtResumeProcess(process: HANDLE) -> i32;
    }

    /// Anvil's own job, kept open for the life of the process.
    pub static OWN_JOB: std::sync::OnceLock<Job> = std::sync::OnceLock::new();

    /// A job object. With no limits set its processes outlive its handle, and none may leave it.
    pub struct Job {
        pub handle: HANDLE,
    }

    // A job handle may be used from any thread
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        pub fn create() -> Option<Job> {
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            (!handle.is_null()).then_some(Job { handle })
        }

        /// Puts a process in the job; false when Windows refuses (a job that allows no nesting).
        pub fn assign(&self, child: &Child) -> bool {
            unsafe { AssignProcessToJobObject(self.handle, child.as_raw_handle() as HANDLE) != 0 }
        }

        pub fn terminate(&self) {
            unsafe { TerminateJobObject(self.handle, 1) };
        }

        /// How many processes in the job are running.
        pub fn active_processes(&self) -> u32 {
            let mut information: JOBOBJECT_BASIC_ACCOUNTING_INFORMATION = unsafe { std::mem::zeroed() };
            let succeeded = unsafe {
                QueryInformationJobObject(
                    self.handle,
                    JobObjectBasicAccountingInformation,
                    &mut information as *mut _ as *mut std::ffi::c_void,
                    std::mem::size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                    std::ptr::null_mut(),
                )
            };
            if succeeded == 0 {
                return 0;
            }

            information.ActiveProcesses
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

    /// Resumes a process started suspended.
    pub fn resume(child: &Child) -> io::Result<()> {
        let status = unsafe { NtResumeProcess(child.as_raw_handle() as HANDLE) };
        if status < 0 {
            return Err(io::Error::other(format!("could not resume it (NTSTATUS 0x{:08X})", status as u32)));
        }

        Ok(())
    }
}
