//! Thin wrappers over the Darwin APIs the spec calls for.
//!
//! Memory is always `ri_phys_footprint` (what `top`'s MEM column and Activity
//! Monitor show), never RSS: RSS drops toward zero once pages are compressed
//! or swapped out, which is exactly the leak case portman exists to catch.

use std::ffi::CString;
use std::mem::{size_of, MaybeUninit};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use libc::{c_int, c_void};

use crate::model::{PressureLevel, Vitals};

pub fn all_pids() -> Vec<i32> {
    unsafe {
        let n = libc::proc_listallpids(std::ptr::null_mut(), 0);
        if n <= 0 {
            return Vec::new();
        }
        // Headroom for processes spawned between the two calls.
        let mut buf = vec![0i32; n as usize + 64];
        let got = libc::proc_listallpids(
            buf.as_mut_ptr().cast(),
            (buf.len() * size_of::<i32>()) as c_int,
        );
        if got <= 0 {
            return Vec::new();
        }
        buf.truncate(got as usize);
        buf.retain(|&p| p > 0);
        buf
    }
}

#[derive(Debug, Clone)]
pub struct BsdInfo {
    pub ppid: i32,
    pub uid: u32,
    pub comm: String,
    pub started_at: SystemTime,
}

pub fn bsd_info(pid: i32) -> Option<BsdInfo> {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if n != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    // pbi_name holds up to 32 chars; pbi_comm is truncated to 16.
    let name = c_chars(&info.pbi_name);
    let comm = if name.is_empty() { c_chars(&info.pbi_comm) } else { name };
    Some(BsdInfo {
        ppid: info.pbi_ppid as i32,
        uid: info.pbi_uid,
        comm,
        started_at: UNIX_EPOCH
            + Duration::from_secs(info.pbi_start_tvsec)
            + Duration::from_micros(info.pbi_start_tvusec),
    })
}

fn c_chars(chars: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = chars.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

#[derive(Debug, Clone, Copy)]
pub struct Rusage {
    pub footprint: u64,
    /// Lifetime user + system CPU, nanoseconds.
    pub cpu_ns: u64,
}

/// Mach timebase: rusage CPU times are in mach absolute-time ticks, which
/// are 1 ns on Intel but 125/3 ns on Apple Silicon.
static TIMEBASE: LazyLock<(u64, u64)> = LazyLock::new(|| {
    let mut tb = mach2::mach_time::mach_timebase_info { numer: 0, denom: 0 };
    unsafe { mach2::mach_time::mach_timebase_info(&mut tb) };
    if tb.denom == 0 {
        (1, 1)
    } else {
        (tb.numer as u64, tb.denom as u64)
    }
});

pub fn rusage(pid: i32) -> Option<Rusage> {
    let mut ri = MaybeUninit::<libc::rusage_info_v4>::zeroed();
    let rc = unsafe {
        libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, ri.as_mut_ptr().cast())
    };
    if rc != 0 {
        return None;
    }
    let ri = unsafe { ri.assume_init() };
    let (numer, denom) = *TIMEBASE;
    let ticks = ri.ri_user_time.saturating_add(ri.ri_system_time) as u128;
    Some(Rusage {
        footprint: ri.ri_phys_footprint,
        cpu_ns: (ticks * numer as u128 / denom as u128) as u64,
    })
}

pub fn cwd(pid: i32) -> Option<PathBuf> {
    let mut info = MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let size = size_of::<libc::proc_vnodepathinfo>() as c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDVNODEPATHINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if n != size {
        return None;
    }
    let info = unsafe { info.assume_init() };
    // libc spells `char vip_path[MAXPATHLEN]` as 32×32 chunks.
    let path = c_chars(info.pvi_cdir.vip_path.as_flattened());
    (!path.is_empty()).then(|| PathBuf::from(path))
}

pub fn exe_path(pid: i32) -> Option<String> {
    let mut buf = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
    let n = unsafe { libc::proc_pidpath(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    if n <= 0 {
        return None;
    }
    buf.truncate(n as usize);
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Full argv joined with spaces, via `KERN_PROCARGS2`. Only readable for
/// processes we own.
pub fn cmdline(pid: i32) -> Option<String> {
    let argmax = sysctl_value::<c_int>("kern.argmax").unwrap_or(1 << 20) as usize;
    let mut buf = vec![0u8; argmax];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let rc = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if rc != 0 || size < size_of::<c_int>() {
        return None;
    }
    buf.truncate(size);
    parse_procargs2(&buf)
}

/// Layout: `argc: i32`, exec path, NUL padding, then `argc` NUL-terminated args.
fn parse_procargs2(buf: &[u8]) -> Option<String> {
    let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?) as usize;
    let rest = &buf[4..];
    let exec_end = rest.iter().position(|&b| b == 0)?;
    let mut i = exec_end;
    while i < rest.len() && rest[i] == 0 {
        i += 1;
    }
    let args: Vec<String> = rest[i..]
        .split(|&b| b == 0)
        .take(argc)
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
    Some(args.join(" "))
}

/// TCP ports this process has in LISTEN, via `PROC_PIDLISTFDS` +
/// `PROC_PIDFDSOCKETINFO`. `None` means libproc failed (fall back to lsof).
pub fn listening_ports(pid: i32, nfiles_hint: usize) -> Option<Vec<u16>> {
    use libproc::libproc::file_info::{pidfdinfo, ListFDs, ProcFDType};
    use libproc::libproc::net_info::{SocketFDInfo, SocketInfoKind, TcpSIState};
    use libproc::libproc::proc_pid::listpidinfo;

    let fds = listpidinfo::<ListFDs>(pid, nfiles_hint.max(64)).ok()?;
    let mut ports = Vec::new();
    for fd in fds {
        if !matches!(ProcFDType::from(fd.proc_fdtype), ProcFDType::Socket) {
            continue;
        }
        let Ok(sock) = pidfdinfo::<SocketFDInfo>(pid, fd.proc_fd) else { continue };
        if !matches!(SocketInfoKind::from(sock.psi.soi_kind), SocketInfoKind::Tcp) {
            continue;
        }
        let tcp = unsafe { sock.psi.soi_proto.pri_tcp };
        if !matches!(TcpSIState::from(tcp.tcpsi_state), TcpSIState::Listen) {
            continue;
        }
        let port = u16::from_be(tcp.tcpsi_ini.insi_lport as u16);
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    Some(ports)
}

/// Open-file count, used to size the fd list buffer.
pub fn nfiles(pid: i32) -> usize {
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
    };
    if n != size {
        return 256;
    }
    unsafe { info.assume_init() }.pbi_nfiles as usize
}

/// Exists and isn't a zombie (an exited child nobody has reaped yet).
pub fn is_alive(pid: i32) -> bool {
    let rc = unsafe { libc::kill(pid, 0) };
    if rc != 0 && std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM) {
        return false;
    }
    let mut info = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = size_of::<libc::proc_bsdinfo>() as c_int;
    let n = unsafe {
        libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, info.as_mut_ptr().cast(), size)
    };
    // libproc has nothing to say about zombies: n == 0.
    n == size && unsafe { info.assume_init() }.pbi_status != libc::SZOMB
}

pub fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

// ── vitals ───────────────────────────────────────────────────────────────────

fn sysctl_value<T: Copy>(name: &str) -> Option<T> {
    let cname = CString::new(name).ok()?;
    let mut val = MaybeUninit::<T>::zeroed();
    let mut size = size_of::<T>();
    let rc = unsafe {
        libc::sysctlbyname(cname.as_ptr(), val.as_mut_ptr().cast::<c_void>(), &mut size, std::ptr::null_mut(), 0)
    };
    (rc == 0 && size == size_of::<T>()).then(|| unsafe { val.assume_init() })
}

pub fn vitals() -> Vitals {
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) }.max(4096) as u64;
    let ram_total = sysctl_value::<u64>("hw.memsize").unwrap_or(0);

    let (mut ram_free, mut compressed) = (0, 0);
    unsafe {
        let mut stats = MaybeUninit::<libc::vm_statistics64>::zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        #[allow(deprecated)]
        let host = libc::mach_host_self();
        let kr = libc::host_statistics64(host, libc::HOST_VM_INFO64, stats.as_mut_ptr().cast(), &mut count);
        if kr == libc::KERN_SUCCESS {
            let s = stats.assume_init();
            ram_free = s.free_count as u64 * page;
            compressed = s.compressor_page_count as u64 * page;
        }
    }

    let (swap_used, swap_total) = sysctl_value::<libc::xsw_usage>("vm.swapusage")
        .map(|x| (x.xsu_used, x.xsu_total))
        .unwrap_or((0, 0));

    let pressure = match sysctl_value::<c_int>("kern.memorystatus_vm_pressure_level") {
        Some(4) => PressureLevel::Critical,
        Some(2) => PressureLevel::Warn,
        _ => PressureLevel::Normal,
    };

    Vitals {
        pressure,
        ram_total,
        ram_free,
        compressed,
        swap_used,
        swap_total,
        disk_free: disk_free("/System/Volumes/Data").or_else(|| disk_free("/")).unwrap_or(0),
    }
}

/// Available bytes on a volume. The sealed `/` system volume is misleading,
/// so callers ask for the Data volume first.
pub fn disk_free(path: &str) -> Option<u64> {
    let cpath = CString::new(path).ok()?;
    let mut st = MaybeUninit::<libc::statfs>::zeroed();
    let rc = unsafe { libc::statfs(cpath.as_ptr(), st.as_mut_ptr()) };
    if rc != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    Some(st.f_bavail * st.f_bsize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_procargs() {
        let mut buf = 3i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(b"/usr/bin/node\0\0\0node\0server.js\0--port\0PATH=/x\0");
        assert_eq!(parse_procargs2(&buf).unwrap(), "node server.js --port");
    }

    #[test]
    fn samples_self() {
        let me = std::process::id() as i32;
        let info = bsd_info(me).expect("bsd info for self");
        assert_eq!(info.uid, current_uid());
        let ru = rusage(me).expect("rusage for self");
        assert!(ru.footprint > 0);
        assert!(cwd(me).is_some());
        assert!(cmdline(me).is_some());
        assert!(all_pids().contains(&me));
    }

    #[test]
    fn finds_own_listener() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let me = std::process::id() as i32;
        let ports = listening_ports(me, nfiles(me)).unwrap();
        assert!(ports.contains(&port), "{ports:?} missing {port}");
    }

    #[test]
    fn vitals_are_sane() {
        let v = vitals();
        assert!(v.ram_total > 0);
        assert!(v.disk_free > 0);
    }
}
