//! Real system metrics, sampled on a background thread once per second
//! (a slow OS query never blocks the media tick). Every value is measured;
//! anything the OS cannot report is `None` and shown as "Unavailable".
//!
//! * Process CPU / memory: `GetProcessTimes` / `K32GetProcessMemoryInfo`
//!   (Windows), `/proc/self` (Linux).
//! * System CPU / memory: `GetSystemTimes` / `GlobalMemoryStatusEx`,
//!   `/proc/stat` / `/proc/meminfo`.
//! * GPU (Windows): the PDH counter `\GPU Engine(*)\Utilization Percentage`
//!   — the source Task Manager uses. Overall = busiest engine (sum over
//!   processes per engine); "this app" = engines used by our process;
//!   video encode = the busiest VideoEncode engine.

use serde::Serialize;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, Default, Serialize)]
pub struct SysStats {
    /// Process CPU, % of the whole machine (all cores = 100 %).
    pub cpu_percent: f32,
    /// Process resident memory (working set), MB.
    pub ram_mb: f32,
    /// Logical cores.
    pub cores: usize,
    /// Whole-machine CPU, %.
    pub system_cpu_percent: Option<f32>,
    /// Physical memory in use / total, MB.
    pub mem_used_mb: Option<f32>,
    pub mem_total_mb: Option<f32>,
    /// GPU: busiest engine, %.
    pub gpu_percent: Option<f32>,
    /// GPU used by this app (sum over our engines' busiest), %.
    pub gpu_app_percent: Option<f32>,
    /// Hardware video-encode engine, % (NVENC / Quick Sync / AMF).
    pub gpu_encode_percent: Option<f32>,
    /// Why GPU metrics are unavailable, if they are.
    pub gpu_note: Option<String>,
}

/// Background sampler handle.
pub(crate) struct SysMonitor {
    latest: Arc<Mutex<SysStats>>,
}

impl SysMonitor {
    pub(crate) fn start() -> Self {
        let latest = Arc::new(Mutex::new(SysStats::default()));
        let out = Arc::clone(&latest);
        let _ = std::thread::Builder::new().name("sf-sysmon".into()).spawn(move || {
            let mut s = SysSampler::new();
            loop {
                std::thread::sleep(std::time::Duration::from_millis(1000));
                // The handle owner is gone (shell dropped): stop.
                if Arc::strong_count(&out) == 1 {
                    break;
                }
                let st = s.sample();
                if let Ok(mut l) = out.lock() {
                    *l = st;
                }
            }
        });
        Self { latest }
    }

    pub(crate) fn latest(&self) -> SysStats {
        self.latest.lock().map(|l| l.clone()).unwrap_or_default()
    }
}

/// Synchronous sampler (one instance per monitor thread).
pub(crate) struct SysSampler {
    last_wall: std::time::Instant,
    last_cpu_s: f64,
    last_sys: Option<(u64, u64)>, // (idle, total) ticks
    cores: usize,
    #[cfg(windows)]
    gpu: Option<win::GpuQuery>,
    #[cfg(windows)]
    gpu_err: Option<String>,
}

impl SysSampler {
    pub(crate) fn new() -> Self {
        #[cfg(windows)]
        let (gpu, gpu_err) = match win::GpuQuery::open() {
            Ok(q) => (Some(q), None),
            Err(e) => (None, Some(e)),
        };
        Self {
            last_wall: std::time::Instant::now(),
            last_cpu_s: process_cpu_seconds().unwrap_or(0.0),
            last_sys: system_cpu_ticks(),
            cores: std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1),
            #[cfg(windows)]
            gpu,
            #[cfg(windows)]
            gpu_err,
        }
    }

    pub(crate) fn sample(&mut self) -> SysStats {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.last_wall).as_secs_f64().max(1e-3);
        let mut st = SysStats { cores: self.cores, ..Default::default() };
        if let Some(cpu) = process_cpu_seconds() {
            st.cpu_percent = ((cpu - self.last_cpu_s) / (dt * self.cores as f64) * 100.0).clamp(0.0, 100.0) as f32;
            self.last_cpu_s = cpu;
        }
        st.ram_mb = process_rss_bytes().unwrap_or(0) as f32 / (1024.0 * 1024.0);
        if let (Some((i0, t0)), Some((i1, t1))) = (self.last_sys, system_cpu_ticks()) {
            if t1 > t0 {
                let busy = 1.0 - (i1.saturating_sub(i0)) as f64 / (t1 - t0) as f64;
                st.system_cpu_percent = Some((busy * 100.0).clamp(0.0, 100.0) as f32);
            }
            self.last_sys = Some((i1, t1));
        } else {
            self.last_sys = system_cpu_ticks();
        }
        if let Some((used, total)) = system_memory_bytes() {
            st.mem_used_mb = Some(used as f32 / 1048576.0);
            st.mem_total_mb = Some(total as f32 / 1048576.0);
        }
        #[cfg(windows)]
        {
            match self.gpu.as_mut().map(|q| q.sample()) {
                Some(Ok(g)) => {
                    st.gpu_percent = Some(g.overall);
                    st.gpu_app_percent = Some(g.app);
                    st.gpu_encode_percent = g.encode;
                }
                Some(Err(e)) => st.gpu_note = Some(e),
                None => st.gpu_note = self.gpu_err.clone(),
            }
        }
        #[cfg(not(windows))]
        {
            st.gpu_note = Some("GPU utilisation is read from Windows performance counters; not available on this OS".into());
        }
        self.last_wall = now;
        st
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
pub(crate) fn process_cpu_seconds() -> Option<f64> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let utime: f64 = f.get(11)?.parse().ok()?;
    let stime: f64 = f.get(12)?.parse().ok()?;
    Some((utime + stime) / 100.0) // USER_HZ is 100 on Linux
}

#[cfg(target_os = "linux")]
fn process_rss_bytes() -> Option<u64> {
    let statm = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages * 4096)
}

#[cfg(target_os = "linux")]
fn system_cpu_ticks() -> Option<(u64, u64)> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    let line = stat.lines().next()?;
    let v: Vec<u64> = line.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    if v.len() < 4 {
        return None;
    }
    let idle = v[3] + v.get(4).copied().unwrap_or(0); // idle + iowait
    Some((idle, v.iter().take(8).sum()))
}

#[cfg(target_os = "linux")]
fn system_memory_bytes() -> Option<(u64, u64)> {
    let m = std::fs::read_to_string("/proc/meminfo").ok()?;
    let get = |k: &str| -> Option<u64> {
        m.lines()
            .find(|l| l.starts_with(k))?
            .split_whitespace()
            .nth(1)?
            .parse::<u64>()
            .ok()
            .map(|kb| kb * 1024)
    };
    let total = get("MemTotal:")?;
    let avail = get("MemAvailable:")?;
    Some((total.saturating_sub(avail), total))
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

#[cfg(any(windows, sf_wincheck))]
#[allow(dead_code, non_snake_case)]
mod win {
    #[repr(C)]
    #[derive(Default, Clone, Copy)]
    pub struct FileTime {
        pub low: u32,
        pub high: u32,
    }
    impl FileTime {
        pub fn ticks(self) -> u64 {
            (self.high as u64) << 32 | self.low as u64
        }
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct ProcessMemoryCounters {
        pub cb: u32,
        pub page_fault_count: u32,
        pub peak_working_set_size: usize,
        pub working_set_size: usize,
        pub quota_peak_paged_pool_usage: usize,
        pub quota_paged_pool_usage: usize,
        pub quota_peak_non_paged_pool_usage: usize,
        pub quota_non_paged_pool_usage: usize,
        pub pagefile_usage: usize,
        pub peak_pagefile_usage: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    pub struct MemoryStatusEx {
        pub length: u32,
        pub memory_load: u32,
        pub total_phys: u64,
        pub avail_phys: u64,
        pub total_page_file: u64,
        pub avail_page_file: u64,
        pub total_virtual: u64,
        pub avail_virtual: u64,
        pub avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GetCurrentProcess() -> isize;
        pub fn GetCurrentProcessId() -> u32;
        pub fn GetProcessTimes(h: isize, c: *mut FileTime, e: *mut FileTime, k: *mut FileTime, u: *mut FileTime) -> i32;
        pub fn K32GetProcessMemoryInfo(h: isize, c: *mut ProcessMemoryCounters, cb: u32) -> i32;
        pub fn GetSystemTimes(idle: *mut FileTime, kernel: *mut FileTime, user: *mut FileTime) -> i32;
        pub fn GlobalMemoryStatusEx(m: *mut MemoryStatusEx) -> i32;
    }

    // PDH (Performance Data Helper).
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PdhFmtCounterValue {
        pub c_status: u32,
        pub _pad: u32,
        pub double_value: f64,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub struct PdhFmtCounterValueItemW {
        pub name: *const u16,
        pub value: PdhFmtCounterValue,
    }
    pub const PDH_FMT_DOUBLE: u32 = 0x0000_0200;
    pub const PDH_FMT_NOCAP100: u32 = 0x0000_8000;
    pub const PDH_MORE_DATA: u32 = 0x8000_07D2;
    pub const PDH_CSTATUS_VALID_DATA: u32 = 0;
    pub const PDH_CSTATUS_NEW_DATA: u32 = 1;
    #[link(name = "pdh")]
    extern "system" {
        pub fn PdhOpenQueryW(src: *const u16, user: usize, query: *mut isize) -> u32;
        pub fn PdhAddEnglishCounterW(query: isize, path: *const u16, user: usize, counter: *mut isize) -> u32;
        pub fn PdhCollectQueryData(query: isize) -> u32;
        pub fn PdhGetFormattedCounterArrayW(
            counter: isize,
            format: u32,
            buffer_size: *mut u32,
            item_count: *mut u32,
            items: *mut PdhFmtCounterValueItemW,
        ) -> u32;
        pub fn PdhCloseQuery(query: isize) -> u32;
    }

    pub struct GpuSample {
        pub overall: f32,
        pub app: f32,
        pub encode: Option<f32>,
    }

    pub struct GpuQuery {
        query: isize,
        counter: isize,
        pid_tag: String,
    }

    // SAFETY: the PDH handles are only used from the sampler thread.
    unsafe impl Send for GpuQuery {}

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    impl GpuQuery {
        pub fn open() -> Result<Self, String> {
            let mut query = 0isize;
            // SAFETY: valid out-pointer; null data source = live data.
            let r = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &mut query) };
            if r != 0 {
                return Err(format!("GPU counters unavailable (PdhOpenQuery 0x{r:08X})"));
            }
            let path = wide("\\GPU Engine(*)\\Utilization Percentage");
            let mut counter = 0isize;
            // SAFETY: open query handle, NUL-terminated path, out-pointer.
            let r = unsafe { PdhAddEnglishCounterW(query, path.as_ptr(), 0, &mut counter) };
            if r != 0 {
                // SAFETY: handle from PdhOpenQueryW.
                unsafe { PdhCloseQuery(query) };
                return Err(format!(
                    "GPU counters unavailable on this system (\\GPU Engine counter: 0x{r:08X}; needs Windows 10 1709+ and a WDDM 2.x driver)"
                ));
            }
            // SAFETY: valid query; first collect primes the rate counter.
            unsafe { PdhCollectQueryData(query) };
            // SAFETY: plain Win32 call.
            let pid = unsafe { GetCurrentProcessId() };
            Ok(Self { query, counter, pid_tag: format!("pid_{pid}_") })
        }

        pub fn sample(&mut self) -> Result<GpuSample, String> {
            // SAFETY: valid query handle.
            let r = unsafe { PdhCollectQueryData(self.query) };
            if r != 0 {
                return Err(format!("GPU counter collect failed (0x{r:08X})"));
            }
            let mut size = 0u32;
            let mut count = 0u32;
            // SAFETY: size query with a null buffer.
            let r = unsafe {
                PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, &mut size, &mut count, std::ptr::null_mut())
            };
            if r != PDH_MORE_DATA || size == 0 {
                return Err(format!("GPU counter has no instances (0x{r:08X})"));
            }
            // u64 backing keeps the item array 8-byte aligned.
            let mut buf = vec![0u64; (size as usize).div_ceil(8) + 1];
            let items = buf.as_mut_ptr() as *mut PdhFmtCounterValueItemW;
            // SAFETY: buffer of at least `size` bytes, aligned for the items.
            let r = unsafe { PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE | PDH_FMT_NOCAP100, &mut size, &mut count, items) };
            if r != 0 {
                return Err(format!("GPU counter read failed (0x{r:08X})"));
            }
            // SAFETY: PDH filled `count` items; names point into `buf`.
            let items = unsafe { std::slice::from_raw_parts(items, count as usize) };
            let mut per_engine: std::collections::HashMap<String, f64> = Default::default();
            let mut app_engine: std::collections::HashMap<String, f64> = Default::default();
            let mut encode: Option<f64> = None;
            let mut enc_engine: std::collections::HashMap<String, f64> = Default::default();
            for it in items {
                if it.value.c_status != PDH_CSTATUS_VALID_DATA && it.value.c_status != PDH_CSTATUS_NEW_DATA {
                    continue;
                }
                if it.name.is_null() {
                    continue;
                }
                // SAFETY: NUL-terminated UTF-16 string inside `buf`.
                let name = unsafe {
                    let mut n = 0;
                    while *it.name.add(n) != 0 {
                        n += 1;
                    }
                    String::from_utf16_lossy(std::slice::from_raw_parts(it.name, n))
                };
                let v = it.value.double_value.max(0.0);
                // "pid_1234_luid_0x..._phys_0_eng_3_engtype_3D": the engine
                // key is everything after the pid part.
                let engine = name.split_once("_luid_").map(|(_, e)| e.to_string()).unwrap_or_else(|| name.clone());
                *per_engine.entry(engine.clone()).or_default() += v;
                if name.starts_with(&self.pid_tag) {
                    *app_engine.entry(engine.clone()).or_default() += v;
                }
                if engine.contains("engtype_VideoEncode") {
                    *enc_engine.entry(engine).or_default() += v;
                }
            }
            for v in enc_engine.values() {
                encode = Some(encode.map_or(*v, |e: f64| e.max(*v)));
            }
            let max = |m: &std::collections::HashMap<String, f64>| m.values().cloned().fold(0.0f64, f64::max);
            Ok(GpuSample {
                overall: max(&per_engine).min(100.0) as f32,
                app: max(&app_engine).min(100.0) as f32,
                encode: encode.map(|e| e.min(100.0) as f32),
            })
        }
    }

    impl Drop for GpuQuery {
        fn drop(&mut self) {
            // SAFETY: handle from PdhOpenQueryW.
            unsafe { PdhCloseQuery(self.query) };
        }
    }
}

#[cfg(windows)]
pub(crate) fn process_cpu_seconds() -> Option<f64> {
    let (mut c, mut e, mut k, mut u) = Default::default();
    // SAFETY: current-process pseudo handle, valid out-pointers.
    let ok = unsafe { win::GetProcessTimes(win::GetCurrentProcess(), &mut c, &mut e, &mut k, &mut u) };
    (ok != 0).then(|| (k.ticks() + u.ticks()) as f64 / 1e7)
}

#[cfg(windows)]
fn process_rss_bytes() -> Option<u64> {
    let mut pmc = win::ProcessMemoryCounters { cb: std::mem::size_of::<win::ProcessMemoryCounters>() as u32, ..Default::default() };
    // SAFETY: `cb` set to the struct size as required.
    let ok = unsafe { win::K32GetProcessMemoryInfo(win::GetCurrentProcess(), &mut pmc, pmc.cb) };
    (ok != 0).then_some(pmc.working_set_size as u64)
}

#[cfg(windows)]
fn system_cpu_ticks() -> Option<(u64, u64)> {
    let (mut i, mut k, mut u) = Default::default();
    // SAFETY: valid out-pointers. Kernel time includes idle time.
    let ok = unsafe { win::GetSystemTimes(&mut i, &mut k, &mut u) };
    let (i, k, u): (win::FileTime, win::FileTime, win::FileTime) = (i, k, u);
    (ok != 0).then(|| (i.ticks(), k.ticks() + u.ticks()))
}

#[cfg(windows)]
fn system_memory_bytes() -> Option<(u64, u64)> {
    let mut m = win::MemoryStatusEx { length: std::mem::size_of::<win::MemoryStatusEx>() as u32, ..Default::default() };
    // SAFETY: `length` set as the API requires.
    let ok = unsafe { win::GlobalMemoryStatusEx(&mut m) };
    (ok != 0).then(|| (m.total_phys.saturating_sub(m.avail_phys), m.total_phys))
}

#[cfg(not(any(windows, target_os = "linux")))]
pub(crate) fn process_cpu_seconds() -> Option<f64> {
    None
}
#[cfg(not(any(windows, target_os = "linux")))]
fn process_rss_bytes() -> Option<u64> {
    None
}
#[cfg(not(any(windows, target_os = "linux")))]
fn system_cpu_ticks() -> Option<(u64, u64)> {
    None
}
#[cfg(not(any(windows, target_os = "linux")))]
fn system_memory_bytes() -> Option<(u64, u64)> {
    None
}
