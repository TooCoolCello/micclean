//! The Windows plugin host: searching, loading and running VST3 and CLAP plugins, each in a
//! child process of its own, and their editor windows.

use super::{Entry, Format, Info, Procs, Search};
use crate::dsp::{FRAME, SR};
use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{clap_event_header, clap_input_events, clap_output_events};
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_audio_port_info, clap_plugin_audio_ports};
use clap_sys::ext::gui::{CLAP_EXT_GUI, CLAP_WINDOW_API_WIN32, clap_host_gui, clap_plugin_gui, clap_window, clap_window_handle};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_host_params, clap_param_clear_flags, clap_param_rescan_flags};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state, clap_plugin_state};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::id::clap_id;
use clap_sys::plugin::clap_plugin;
use clap_sys::process::{CLAP_PROCESS_ERROR, clap_process};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::version::CLAP_VERSION;
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell, UnsafeCell};
use std::ffi::{CStr, CString, c_char, c_void};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicUsize, Ordering::Acquire, Ordering::Relaxed, Ordering::Release};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};
use vst3::Steinberg::Vst::*;
use vst3::Steinberg::*;
use vst3::{Class, ComPtr, ComRef, ComWrapper, Interface};
use windows_sys::Win32::Foundation::{FreeLibrary, HANDLE, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows_sys::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows_sys::Win32::UI::WindowsAndMessaging as wm;

// ---------------------------------------------------------------------------------------------
// Searching

/// The usual folders, both formats: where installers put plugins.
pub fn default_dirs() -> Vec<PathBuf> {
    let mut v = dirs(Format::Vst3);
    v.extend(dirs(Format::Clap));
    v
}

fn dirs(format: Format) -> Vec<PathBuf> {
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let (common, local) = (env("CommonProgramFiles"), env("LOCALAPPDATA").map(|d| d.join("Programs").join("Common")));
    let sub = format.name();
    let mut v: Vec<PathBuf> = match format {
        Format::Clap => std::env::var_os("CLAP_PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default(),
        Format::Vst3 => Vec::new(),
    };
    v.extend([common, local].into_iter().flatten().map(|d| d.join(sub)));
    v
}

/// Every plugin file in the usual folders and in `extra` (folders you added, both formats).
pub fn files(extra: &[PathBuf]) -> Vec<(Format, PathBuf)> {
    let mut out = Vec::new();
    for f in [Format::Vst3, Format::Clap] {
        for d in dirs(f).iter().chain(extra) {
            walk(d, f, &mut out, 0);
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk(dir: &Path, f: Format, out: &mut Vec<(Format, PathBuf)>, depth: u32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
        match (f, ext.as_deref(), p.is_dir()) {
            // A bundle: the DLL for this machine is inside.
            (Format::Vst3, Some("vst3"), true) => {
                let Ok(inner) = std::fs::read_dir(p.join("Contents").join("x86_64-win")) else { continue };
                out.extend(inner.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("vst3"))).map(|p| (f, p)));
            }
            (Format::Vst3, Some("vst3"), false) | (Format::Clap, Some("clap"), false) => out.push((f, p)),
            (_, _, true) if depth < 6 => walk(&p, f, out, depth + 1),
            _ => {}
        }
    }
}

/// `micclean --scan-plugin <vst3|clap> <file>`: prints the file's effects as JSON lines.
pub fn scan_main(args: &[String]) {
    use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode};
    // A plugin that crashes here just ends this copy: no "stopped working" box to wait on.
    unsafe { SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX) };
    let (Some(f), Some(path)) = (args.first(), args.get(1)) else { return };
    let format = if f == "clap" { Format::Clap } else { Format::Vst3 };
    match scan_file(format, Path::new(path)) {
        Ok(list) => {
            for i in list {
                println!("{}", serde_json::to_string(&i).unwrap_or_default());
            }
        }
        Err(e) => eprintln!("{e}"),
    }
}

/// The effects in one plugin file. Loads it, so the search runs this in a child process.
pub fn scan_file(format: Format, path: &Path) -> Result<Vec<Info>, String> {
    let lib = Lib::open(path, format)?;
    // SAFETY: the library stays loaded for these calls, and both read only what the plugin
    // hands back through its own factory.
    let list = unsafe {
        match format {
            Format::Vst3 => vst3_classes(&lib)?,
            Format::Clap => clap_descriptors(&lib)?,
        }
    };
    Ok(list.into_iter().map(|(uid, name, vendor)| Info { format, path: path.into(), uid, name, vendor }).collect())
}

/// Searches every plugin file, four at a time, each in its own child process. One that hangs
/// (a licence window nobody sees) is given up after 20 s.
pub fn search(extra: Vec<PathBuf>) -> Arc<Mutex<Search>> {
    let s = Arc::new(Mutex::new(Search::default()));
    let out = s.clone();
    std::thread::spawn(move || {
        let Ok(exe) = std::env::current_exe() else { return };
        let files = files(&extra);
        let total = files.len();
        out.lock().unwrap().progress = (0, total);
        let (next, done) = (AtomicUsize::new(0), AtomicUsize::new(0));
        let found = Mutex::new(Vec::new());
        std::thread::scope(|sc| {
            for _ in 0..4 {
                sc.spawn(|| {
                    while let Some((f, path)) = files.get(next.fetch_add(1, Relaxed)) {
                        let list = scan_child(&exe, *f, path);
                        found.lock().unwrap().extend(list);
                        let n = done.fetch_add(1, Relaxed) + 1;
                        out.lock().unwrap().progress = (n, total);
                    }
                });
            }
        });
        let mut found = found.into_inner().unwrap();
        found.sort_by_key(|i| (i.name.to_lowercase(), i.format));
        out.lock().unwrap().found = Some(found);
    });
    s
}

/// Windows' own folder picker, owned by `owner`. None if cancelled.
pub fn pick_folder(owner: Option<isize>) -> Option<PathBuf> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
    use windows::Win32::UI::Shell::{FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH};
    // SAFETY: COM on the GUI thread, which is already a single-threaded apartment (drag and
    // drop); the extra initialise is balanced below. The returned string is freed once copied.
    unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let pick = || -> Option<PathBuf> {
            let d: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
            d.SetOptions(d.GetOptions().ok()? | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
            let _ = d.SetTitle(windows::core::w!("Add a plugin folder"));
            d.Show(owner.map(|h| HWND(h as *mut c_void))).ok()?;
            let p = d.GetResult().ok()?.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
            let s = p.to_string().ok();
            CoTaskMemFree(Some(p.0 as *const c_void));
            s.map(PathBuf::from)
        };
        let r = pick();
        if init.is_ok() {
            CoUninitialize();
        }
        r
    }
}

fn scan_child(exe: &Path, format: Format, path: &Path) -> Vec<Info> {
    use std::io::Read;
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let arg = if format == Format::Clap { "clap" } else { "vst3" };
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let child = Command::new(exe).arg("--scan-plugin").arg(arg).arg(path).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).creation_flags(CREATE_NO_WINDOW).spawn();
    let Ok(mut child) = child else { return Vec::new() };
    let mut stdout = child.stdout.take().expect("piped above");
    // Read on the side: a big shell plugin's list could fill the pipe before it exits.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match child.try_wait() {
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            Ok(Some(_)) => break,
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    // Something the plugin started could still hold the pipe open: don't wait on it forever.
    let text = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
    text.lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

// ---------------------------------------------------------------------------------------------
// Plugin libraries

/// A loaded plugin DLL, shared by every instance from it (CLAP's init and deinit run once).
struct Lib {
    module: HMODULE,
    format: Format,
    path: PathBuf,
    ready: bool,
}

// SAFETY: a module handle; the plugin calls made through it follow each format's threading rules.
unsafe impl Send for Lib {}
unsafe impl Sync for Lib {}

thread_local! {
    static LIBS: RefCell<Vec<Weak<Lib>>> = const { RefCell::new(Vec::new()) };
}

impl Lib {
    fn open(path: &Path, format: Format) -> Result<Arc<Lib>, String> {
        if let Some(l) = LIBS.with(|l| l.borrow().iter().filter_map(Weak::upgrade).find(|l| l.path == path)) {
            return Ok(l);
        }
        use windows_sys::Win32::System::LibraryLoader::{LOAD_WITH_ALTERED_SEARCH_PATH, LoadLibraryExW};
        // Altered search path: DLLs next to the plugin (its own helpers) are found.
        let module = unsafe { LoadLibraryExW(wide(path.as_os_str()).as_ptr(), null_mut(), LOAD_WITH_ALTERED_SEARCH_PATH) };
        if module.is_null() {
            return Err(format!("Windows couldn't load {} ({})", path.display(), std::io::Error::last_os_error()));
        }
        let mut lib = Lib { module, format, path: path.into(), ready: false };
        // SAFETY: each format's documented entry points, with the signatures they're defined with.
        unsafe {
            match format {
                Format::Vst3 => {
                    if let Some(f) = lib.sym(c"InitDll") {
                        let init: unsafe extern "system" fn() -> bool = std::mem::transmute(f);
                        init();
                    }
                }
                Format::Clap => {
                    let entry = lib.clap_entry().ok_or("Not a CLAP plugin")?;
                    let p = CString::new(path.to_string_lossy().as_bytes()).map_err(|_| "Bad file name")?;
                    if !entry.init.is_some_and(|init| init(p.as_ptr())) {
                        return Err("The plugin didn't start".into());
                    }
                }
            }
        }
        lib.ready = true;
        let lib = Arc::new(lib);
        LIBS.with(|l| {
            let mut l = l.borrow_mut();
            l.retain(|w| w.strong_count() > 0);
            l.push(Arc::downgrade(&lib));
        });
        Ok(lib)
    }

    fn sym(&self, name: &CStr) -> Option<*const c_void> {
        use windows_sys::Win32::System::LibraryLoader::GetProcAddress;
        // SAFETY: a NUL-terminated name and a loaded module.
        unsafe { GetProcAddress(self.module, name.as_ptr() as *const u8).map(|f| f as *const c_void) }
    }

    fn clap_entry(&self) -> Option<&clap_plugin_entry> {
        // SAFETY: CLAP's `clap_entry` export is a clap_plugin_entry that lives as long as the DLL.
        self.sym(c"clap_entry").map(|p| unsafe { &*(p as *const clap_plugin_entry) })
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: every instance from this library is gone (they hold an Arc to it).
        unsafe {
            if self.ready {
                match self.format {
                    Format::Vst3 => {
                        if let Some(f) = self.sym(c"ExitDll") {
                            let exit: unsafe extern "system" fn() -> bool = std::mem::transmute(f);
                            exit();
                        }
                    }
                    Format::Clap => {
                        if let Some(deinit) = self.clap_entry().and_then(|e| e.deinit) {
                            deinit();
                        }
                    }
                }
            }
            FreeLibrary(self.module);
        }
    }
}

fn wide(s: &std::ffi::OsStr) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    s.encode_wide().chain([0]).collect()
}

fn text(b: &[c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_string()
}

/// # Safety
/// `p` is null or a NUL-terminated string.
unsafe fn ctext(p: *const c_char) -> String {
    if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().trim().to_string() }
}

pub fn to_hex(b: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for &x in b {
        s.push(D[(x >> 4) as usize] as char);
        s.push(D[(x & 15) as usize] as char);
    }
    s
}

pub fn from_hex(s: &str) -> Vec<u8> {
    s.as_bytes().chunks_exact(2).filter_map(|c| u8::from_str_radix(std::str::from_utf8(c).ok()?, 16).ok()).collect()
}

/// One saved blob for both VST3 halves: the component's state, then the controller's.
fn join_state(a: &[u8], b: &[u8]) -> Vec<u8> {
    let mut v = (a.len() as u32).to_le_bytes().to_vec();
    v.extend_from_slice(a);
    v.extend_from_slice(b);
    v
}

fn split_state(s: &[u8]) -> (&[u8], &[u8]) {
    let Some((n, rest)) = s.split_first_chunk::<4>() else { return (&[], &[]) };
    let n = (u32::from_le_bytes(*n) as usize).min(rest.len());
    rest.split_at(n)
}

// ---------------------------------------------------------------------------------------------
// VST3 host objects

/// IHostApplication: our name. Messages between a plugin's halves aren't offered.
/// ponytail: plugins that send their controller messages through the host lose those (meters
/// in a split plugin's window); implement IMessage and IAttributeList if one needs it.
struct HostApp;

impl Class for HostApp {
    type Interfaces = (IHostApplication,);
}

impl IHostApplicationTrait for HostApp {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        let dst = unsafe { &mut *name };
        dst.fill(0);
        for (d, c) in dst.iter_mut().zip("MicClean".encode_utf16()) {
            *d = c;
        }
        kResultOk
    }

    unsafe fn createInstance(&self, _cid: *mut TUID, _iid: *mut TUID, obj: *mut *mut c_void) -> tresult {
        if !obj.is_null() {
            unsafe { *obj = null_mut() };
        }
        kNotImplemented
    }
}

/// IComponentHandler: knob turns in the plugin's window, passed on to its audio half.
struct Handler {
    edits: Arc<Mutex<Vec<(u32, f64)>>>,
}

impl Class for Handler {
    type Interfaces = (IComponentHandler,);
}

impl IComponentHandlerTrait for Handler {
    unsafe fn beginEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value: ParamValue) -> tresult {
        if let Ok(mut e) = self.edits.lock()
            && e.len() < 4096
        {
            e.push((id, value));
        }
        kResultOk
    }

    unsafe fn endEdit(&self, _id: ParamID) -> tresult {
        kResultOk
    }

    unsafe fn restartComponent(&self, _flags: int32) -> tresult {
        kResultOk
    }
}

/// IBStream over a byte buffer, for saving and loading a plugin's settings.
struct Stream {
    data: RefCell<Vec<u8>>,
    pos: Cell<usize>,
}

impl Stream {
    fn new(data: &[u8]) -> ComWrapper<Stream> {
        ComWrapper::new(Stream { data: RefCell::new(data.to_vec()), pos: Cell::new(0) })
    }
}

fn stream_ptr(s: &ComWrapper<Stream>) -> *mut IBStream {
    s.as_com_ref::<IBStream>().expect("in its interface list").as_ptr()
}

impl Class for Stream {
    type Interfaces = (IBStream,);
}

impl IBStreamTrait for Stream {
    unsafe fn read(&self, buffer: *mut c_void, n: int32, read: *mut int32) -> tresult {
        let d = self.data.borrow();
        let pos = self.pos.get().min(d.len());
        let k = (n.max(0) as usize).min(d.len() - pos);
        if k > 0 && !buffer.is_null() {
            unsafe { std::ptr::copy_nonoverlapping(d.as_ptr().add(pos), buffer as *mut u8, k) };
        }
        self.pos.set(pos + k);
        if !read.is_null() {
            unsafe { *read = k as int32 };
        }
        kResultOk
    }

    unsafe fn write(&self, buffer: *mut c_void, n: int32, written: *mut int32) -> tresult {
        let n = n.max(0) as usize;
        if n > 0 && buffer.is_null() {
            return kInvalidArgument;
        }
        let mut d = self.data.borrow_mut();
        let pos = self.pos.get();
        if d.len() < pos + n {
            d.resize(pos + n, 0);
        }
        if n > 0 {
            unsafe { std::ptr::copy_nonoverlapping(buffer as *const u8, d.as_mut_ptr().add(pos), n) };
        }
        self.pos.set(pos + n);
        if !written.is_null() {
            unsafe { *written = n as int32 };
        }
        kResultOk
    }

    unsafe fn seek(&self, pos: int64, mode: int32, result: *mut int64) -> tresult {
        use IBStream_::IStreamSeekMode_::*;
        let base = match mode {
            m if m == kIBSeekSet as int32 => 0,
            m if m == kIBSeekCur as int32 => self.pos.get() as i64,
            m if m == kIBSeekEnd as int32 => self.data.borrow().len() as i64,
            _ => return kInvalidArgument,
        };
        let p = base + pos;
        if p < 0 {
            return kInvalidArgument;
        }
        self.pos.set(p as usize);
        if !result.is_null() {
            unsafe { *result = p };
        }
        kResultOk
    }

    unsafe fn tell(&self, pos: *mut int64) -> tresult {
        if pos.is_null() {
            return kInvalidArgument;
        }
        unsafe { *pos = self.pos.get() as int64 };
        kResultOk
    }
}

/// Parameter changes for one block: one value per parameter, at the block's start.
const MAX_CHANGES: usize = 64;

struct Queue {
    id: Cell<u32>,
    value: Cell<f64>,
    points: Cell<i32>,
}

impl Class for Queue {
    type Interfaces = (IParamValueQueue,);
}

impl IParamValueQueueTrait for Queue {
    unsafe fn getParameterId(&self) -> ParamID {
        self.id.get()
    }

    unsafe fn getPointCount(&self) -> int32 {
        self.points.get()
    }

    unsafe fn getPoint(&self, index: int32, offset: *mut int32, value: *mut ParamValue) -> tresult {
        if index != 0 || self.points.get() == 0 || offset.is_null() || value.is_null() {
            return kResultFalse;
        }
        unsafe { (*offset, *value) = (0, self.value.get()) };
        kResultOk
    }

    unsafe fn addPoint(&self, _offset: int32, value: ParamValue, index: *mut int32) -> tresult {
        // One point: the last value wins.
        self.value.set(value);
        self.points.set(1);
        if !index.is_null() {
            unsafe { *index = 0 };
        }
        kResultOk
    }
}

struct Changes {
    queues: Vec<ComWrapper<Queue>>,
    count: Cell<usize>,
}

impl Changes {
    fn new() -> ComWrapper<Changes> {
        let q = || ComWrapper::new(Queue { id: Cell::new(0), value: Cell::new(0.0), points: Cell::new(0) });
        ComWrapper::new(Changes { queues: (0..MAX_CHANGES).map(|_| q()).collect(), count: Cell::new(0) })
    }

    /// The queue for parameter `id`, added if there's room.
    fn queue(&self, id: u32) -> Option<usize> {
        let n = self.count.get();
        if let Some(i) = (0..n).find(|&i| self.queues[i].id.get() == id) {
            return Some(i);
        }
        let q = self.queues.get(n)?;
        q.id.set(id);
        q.points.set(0);
        self.count.set(n + 1);
        Some(n)
    }

    fn ptr(&self, i: usize) -> *mut IParamValueQueue {
        self.queues[i].as_com_ref::<IParamValueQueue>().expect("in its interface list").as_ptr()
    }
}

impl Class for Changes {
    type Interfaces = (IParameterChanges,);
}

impl IParameterChangesTrait for Changes {
    unsafe fn getParameterCount(&self) -> int32 {
        self.count.get() as int32
    }

    unsafe fn getParameterData(&self, index: int32) -> *mut IParamValueQueue {
        if index >= 0 && (index as usize) < self.count.get() { self.ptr(index as usize) } else { null_mut() }
    }

    unsafe fn addParameterData(&self, id: *const ParamID, index: *mut int32) -> *mut IParamValueQueue {
        if id.is_null() {
            return null_mut();
        }
        match self.queue(unsafe { *id }) {
            Some(i) => {
                if !index.is_null() {
                    unsafe { *index = i as int32 };
                }
                self.ptr(i)
            }
            None => null_mut(),
        }
    }
}

/// IPlugFrame: the plugin asks for a new window size.
struct Frame {
    hwnd: Cell<isize>,
    win: Rc<WinState>,
}

impl Class for Frame {
    type Interfaces = (IPlugFrame,);
}

impl IPlugFrameTrait for Frame {
    unsafe fn resizeView(&self, view: *mut IPlugView, size: *mut ViewRect) -> tresult {
        if size.is_null() {
            return kInvalidArgument;
        }
        let mut r = unsafe { *size };
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        // Asked for, so the window's own WM_SIZE needn't pass it back.
        self.win.last.set((w, h));
        set_client_size(self.hwnd.get() as HWND, w, h);
        if let Some(v) = unsafe { ComRef::from_raw(view) } {
            unsafe { v.onSize(&mut r) };
        }
        kResultOk
    }
}

fn parse_uid(s: &str) -> Result<TUID, String> {
    let b = from_hex(s);
    let mut id: TUID = [0; 16];
    if b.len() != 16 {
        return Err("Bad plugin ID".into());
    }
    for (d, x) in id.iter_mut().zip(b) {
        *d = x as c_char;
    }
    Ok(id)
}

/// # Safety
/// `lib` is a loaded VST3 module.
unsafe fn vst3_factory(lib: &Lib) -> Result<ComPtr<IPluginFactory>, String> {
    let f = lib.sym(c"GetPluginFactory").ok_or("Not a VST3 plugin")?;
    let get: unsafe extern "system" fn() -> *mut IPluginFactory = unsafe { std::mem::transmute(f) };
    unsafe { ComPtr::from_raw(get()) }.ok_or_else(|| "The plugin has no factory".into())
}

/// (class ID, name, vendor) of each effect in the module.
///
/// # Safety
/// `lib` is a loaded VST3 module.
unsafe fn vst3_classes(lib: &Lib) -> Result<Vec<(String, String, String)>, String> {
    let factory = unsafe { vst3_factory(lib)? };
    let mut fi: PFactoryInfo = unsafe { std::mem::zeroed() };
    unsafe { factory.getFactoryInfo(&mut fi) };
    let f2 = factory.cast::<IPluginFactory2>();
    let mut out = Vec::new();
    for i in 0..unsafe { factory.countClasses() } {
        let mut ci: PClassInfo = unsafe { std::mem::zeroed() };
        if unsafe { factory.getClassInfo(i, &mut ci) } != kResultOk || text(&ci.category) != "Audio Module Class" {
            continue;
        }
        let (mut sub, mut vendor) = (String::new(), text(&fi.vendor));
        if let Some(f2) = &f2 {
            let mut c2: PClassInfo2 = unsafe { std::mem::zeroed() };
            if unsafe { f2.getClassInfo2(i, &mut c2) } == kResultOk {
                sub = text(&c2.subCategories);
                vendor = Some(text(&c2.vendor)).filter(|v| !v.is_empty()).unwrap_or(vendor);
            }
        }
        // Effects only: an instrument can't take a voice.
        if sub.contains("Instrument") && !sub.contains("Fx") {
            continue;
        }
        out.push((to_hex(&ci.cid.map(|c| c as u8)), text(&ci.name), vendor));
    }
    Ok(out)
}

/// # Safety
/// `cid` names a class of `factory`, which makes objects with interface `I`.
unsafe fn create<I: Interface>(factory: &ComPtr<IPluginFactory>, cid: &TUID) -> Option<ComPtr<I>> {
    let mut obj = null_mut();
    if unsafe { factory.createInstance(cid.as_ptr(), I::IID.as_ptr() as FIDString, &mut obj) } != kResultOk {
        return None;
    }
    unsafe { ComPtr::from_raw(obj as *mut I) }
}

// ---------------------------------------------------------------------------------------------
// CLAP host objects

/// Our side of one CLAP plugin. `host` comes first, so the `*const clap_host` the plugin calls
/// back with is a pointer to this.
#[repr(C)]
struct ClapHost {
    host: clap_host,
    callback: AtomicBool,
    restart: AtomicBool,
    hwnd: AtomicIsize,
}

impl ClapHost {
    fn new() -> Box<ClapHost> {
        let mut h = Box::new(ClapHost {
            host: clap_host {
                clap_version: CLAP_VERSION,
                host_data: null_mut(),
                name: c"MicClean".as_ptr(),
                vendor: c"MicClean".as_ptr(),
                url: c"".as_ptr(),
                version: concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char,
                get_extension: Some(host_extension),
                request_restart: Some(host_restart),
                request_process: Some(host_noop),
                request_callback: Some(host_callback),
            },
            callback: AtomicBool::new(false),
            restart: AtomicBool::new(false),
            hwnd: AtomicIsize::new(0),
        });
        h.host.host_data = &mut *h as *mut ClapHost as *mut c_void;
        h
    }

    /// # Safety
    /// `host` is the `host` field of a live ClapHost.
    unsafe fn of<'a>(host: *const clap_host) -> &'a ClapHost {
        unsafe { &*(host as *const ClapHost) }
    }
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_noop),
    request_resize: Some(host_resize),
    request_show: Some(host_false),
    request_hide: Some(host_false),
    closed: Some(host_closed),
};
static HOST_THREADS: clap_host_thread_check = clap_host_thread_check { is_main_thread: Some(host_is_main), is_audio_thread: Some(host_is_audio) };
static HOST_PARAMS: clap_host_params = clap_host_params { rescan: Some(host_rescan), clear: Some(host_clear), request_flush: Some(host_noop) };
static HOST_LATENCY: clap_host_latency = clap_host_latency { changed: Some(host_noop) };
static HOST_STATE: clap_host_state = clap_host_state { mark_dirty: Some(host_noop) };

unsafe extern "C" fn host_extension(_host: *const clap_host, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return null();
    }
    let id = unsafe { CStr::from_ptr(id) };
    match id {
        _ if id == CLAP_EXT_GUI => &HOST_GUI as *const _ as *const c_void,
        _ if id == CLAP_EXT_THREAD_CHECK => &HOST_THREADS as *const _ as *const c_void,
        _ if id == CLAP_EXT_PARAMS => &HOST_PARAMS as *const _ as *const c_void,
        _ if id == CLAP_EXT_LATENCY => &HOST_LATENCY as *const _ as *const c_void,
        _ if id == CLAP_EXT_STATE => &HOST_STATE as *const _ as *const c_void,
        _ => null(),
    }
}

unsafe extern "C" fn host_noop(_: *const clap_host) {}
unsafe extern "C" fn host_false(_: *const clap_host) -> bool {
    false
}
unsafe extern "C" fn host_closed(_: *const clap_host, _was_destroyed: bool) {}
unsafe extern "C" fn host_rescan(_: *const clap_host, _: clap_param_rescan_flags) {}
unsafe extern "C" fn host_clear(_: *const clap_host, _: clap_id, _: clap_param_clear_flags) {}

unsafe extern "C" fn host_callback(host: *const clap_host) {
    unsafe { ClapHost::of(host) }.callback.store(true, Relaxed);
}

unsafe extern "C" fn host_restart(host: *const clap_host) {
    unsafe { ClapHost::of(host) }.restart.store(true, Relaxed);
}

unsafe extern "C" fn host_is_main(_: *const clap_host) -> bool {
    on_main_thread()
}

unsafe extern "C" fn host_is_audio(_: *const clap_host) -> bool {
    !on_main_thread()
}

unsafe extern "C" fn host_resize(host: *const clap_host, w: u32, h: u32) -> bool {
    let hwnd = unsafe { ClapHost::of(host) }.hwnd.load(Relaxed) as HWND;
    if hwnd.is_null() {
        return false;
    }
    // Asked for, so the window's own WM_SIZE needn't pass it back.
    if let Some(win) = unsafe { win_state(hwnd) } {
        win.last.set((w as i32, h as i32));
    }
    set_client_size(hwnd, w as i32, h as i32);
    true
}

unsafe extern "C" fn no_events(_: *const clap_input_events) -> u32 {
    0
}

unsafe extern "C" fn no_event(_: *const clap_input_events, _: u32) -> *const clap_event_header {
    null()
}

unsafe extern "C" fn drop_event(_: *const clap_output_events, _: *const clap_event_header) -> bool {
    true
}

unsafe extern "C" fn read_bytes(s: *const clap_istream, buf: *mut c_void, size: u64) -> i64 {
    let r = unsafe { &mut *((*s).ctx as *mut (&[u8], usize)) };
    let k = (size as usize).min(r.0.len() - r.1);
    unsafe { std::ptr::copy_nonoverlapping(r.0.as_ptr().add(r.1), buf as *mut u8, k) };
    r.1 += k;
    k as i64
}

unsafe extern "C" fn write_bytes(s: *const clap_ostream, buf: *const c_void, size: u64) -> i64 {
    let v = unsafe { &mut *((*s).ctx as *mut Vec<u8>) };
    v.extend_from_slice(unsafe { std::slice::from_raw_parts(buf as *const u8, size as usize) });
    size as i64
}

/// # Safety
/// `plugin` is a live, initialised CLAP plugin; T is the struct the extension `id` defines.
unsafe fn ext<'a, T>(plugin: *const clap_plugin, id: &CStr) -> Option<&'a T> {
    let get = unsafe { (*plugin).get_extension? };
    unsafe { (get(plugin, id.as_ptr()) as *const T).as_ref() }
}

/// # Safety
/// `lib` is a loaded, initialised CLAP library.
unsafe fn clap_factory(lib: &Lib) -> Result<&clap_plugin_factory, String> {
    let get = lib.clap_entry().and_then(|e| e.get_factory).ok_or("Not a CLAP plugin")?;
    unsafe { (get(CLAP_PLUGIN_FACTORY_ID.as_ptr()) as *const clap_plugin_factory).as_ref() }.ok_or_else(|| "The plugin has no factory".into())
}

/// (ID, name, vendor) of each effect in the library.
///
/// # Safety
/// `lib` is a loaded, initialised CLAP library.
unsafe fn clap_descriptors(lib: &Lib) -> Result<Vec<(String, String, String)>, String> {
    let f = unsafe { clap_factory(lib)? };
    let n = f.get_plugin_count.map_or(0, |c| unsafe { c(f) });
    let mut out = Vec::new();
    for i in 0..n {
        let Some(d) = f.get_plugin_descriptor.and_then(|g| unsafe { g(f, i).as_ref() }) else { continue };
        let mut effect = false;
        let mut p = d.features;
        while !p.is_null() && !unsafe { *p }.is_null() {
            effect |= unsafe { CStr::from_ptr(*p) } == c"audio-effect";
            p = unsafe { p.add(1) };
        }
        if effect {
            out.push(unsafe { (ctext(d.id), ctext(d.name), ctext(d.vendor)) });
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The audio half

/// A plugin's processing, in its process's sound thread, with buffers for all its buses. The
/// first input and output bus carry the voice (copied into each channel, the channels averaged
/// back); any others (a sidechain) get silence.
struct Local {
    run: Run,
    ins: Vec<Vec<f32>>,
    outs: Vec<Vec<f32>>,
    /// Channel pointers, bus after bus; the bus structs point into these.
    _in_ptrs: Vec<*mut f32>,
    _out_ptrs: Vec<*mut f32>,
    main_in: usize,
    main_out: usize,
    started: bool,
    /// Shut down: never called again.
    stopped: bool,
    _lib: Arc<Lib>,
}

enum Run {
    Vst3 {
        proc: ComPtr<IAudioProcessor>,
        in_bus: Vec<AudioBusBuffers>,
        out_bus: Vec<AudioBusBuffers>,
        changes: ComWrapper<Changes>,
        out_changes: ComWrapper<Changes>,
        edits: Arc<Mutex<Vec<(u32, f64)>>>,
        ctx: ProcessContext,
    },
    Clap {
        plugin: *const clap_plugin,
        in_bus: Vec<clap_audio_buffer>,
        out_bus: Vec<clap_audio_buffer>,
        steady: i64,
    },
}

// SAFETY: the sound thread only reaches it through the Mutex, and each format allows process
// calls from a thread other than the one that made the plugin.
unsafe impl Send for Local {}

/// Buffers for buses with these channel counts: the channels, their pointers, and where each
/// bus starts in the pointers.
fn buffers(counts: &[usize]) -> (Vec<Vec<f32>>, Vec<*mut f32>, Vec<usize>) {
    let mut ch: Vec<Vec<f32>> = (0..counts.iter().sum()).map(|_| vec![0.0; FRAME]).collect();
    let ptrs: Vec<*mut f32> = ch.iter_mut().map(|c| c.as_mut_ptr()).collect();
    let starts = counts.iter().scan(0, |s, &n| Some(std::mem::replace(s, *s + n))).collect();
    (ch, ptrs, starts)
}

impl Local {
    /// Runs the plugin on one block of mono sound, in place. A block it fails on stays dry.
    fn process(&mut self, x: &mut [f32]) {
        if self.stopped {
            return;
        }
        let n = x.len().min(FRAME);
        for c in &mut self.ins[..self.main_in] {
            c[..n].copy_from_slice(&x[..n]);
        }
        for c in &mut self.outs {
            c[..n].fill(0.0);
        }
        // SAFETY: the buses point into our own buffers, which hold FRAME samples each; the
        // plugin was set up for blocks of up to FRAME.
        let ok = unsafe {
            match &mut self.run {
                Run::Vst3 { proc, in_bus, out_bus, changes, out_changes, edits, ctx } => {
                    if !self.started {
                        proc.setProcessing(1);
                        self.started = true;
                    }
                    changes.count.set(0);
                    out_changes.count.set(0);
                    if let Ok(mut e) = edits.try_lock() {
                        for (id, v) in e.drain(..) {
                            if let Some(i) = changes.queue(id) {
                                changes.queues[i].addPoint(0, v, null_mut());
                            }
                        }
                    }
                    for b in in_bus.iter_mut().chain(out_bus.iter_mut()) {
                        b.silenceFlags = 0;
                    }
                    let mut data = ProcessData {
                        processMode: ProcessModes_::kRealtime as int32,
                        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
                        numSamples: n as int32,
                        numInputs: in_bus.len() as int32,
                        numOutputs: out_bus.len() as int32,
                        inputs: in_bus.as_mut_ptr(),
                        outputs: out_bus.as_mut_ptr(),
                        inputParameterChanges: changes.as_com_ref::<IParameterChanges>().expect("listed").as_ptr(),
                        outputParameterChanges: out_changes.as_com_ref::<IParameterChanges>().expect("listed").as_ptr(),
                        inputEvents: null_mut(),
                        outputEvents: null_mut(),
                        processContext: ctx,
                    };
                    let r = proc.process(&mut data);
                    ctx.projectTimeSamples += n as i64;
                    ctx.continousTimeSamples += n as i64;
                    r == kResultOk
                }
                Run::Clap { plugin, in_bus, out_bus, steady } => {
                    let p = &**plugin;
                    if !self.started {
                        if !p.start_processing.is_some_and(|f| f(*plugin)) {
                            return;
                        }
                        self.started = true;
                    }
                    let in_ev = clap_input_events { ctx: null_mut(), size: Some(no_events), get: Some(no_event) };
                    let out_ev = clap_output_events { ctx: null_mut(), try_push: Some(drop_event) };
                    let pr = clap_process {
                        steady_time: *steady,
                        frames_count: n as u32,
                        transport: null(),
                        audio_inputs: in_bus.as_ptr(),
                        audio_outputs: out_bus.as_mut_ptr(),
                        audio_inputs_count: in_bus.len() as u32,
                        audio_outputs_count: out_bus.len() as u32,
                        in_events: &in_ev,
                        out_events: &out_ev,
                    };
                    *steady += n as i64;
                    p.process.is_some_and(|f| f(*plugin, &pr) != CLAP_PROCESS_ERROR)
                }
            }
        };
        if ok {
            let k = self.main_out as f32;
            for (i, s) in x[..n].iter_mut().enumerate() {
                *s = self.outs[..self.main_out].iter().map(|c| c[i]).sum::<f32>() / k;
            }
        }
    }

    /// Stops processing for good, on the GUI thread once the sound thread has stopped.
    fn stop(&mut self) {
        if self.started {
            // SAFETY: the plugin is processing, and nothing else calls it now (we hold the lock).
            unsafe {
                match &self.run {
                    Run::Vst3 { proc, .. } => {
                        proc.setProcessing(0);
                    }
                    Run::Clap { plugin, .. } => {
                        if let Some(f) = (**plugin).stop_processing {
                            f(*plugin);
                        }
                    }
                }
            }
            self.started = false;
        }
        self.stopped = true;
    }
}

// ---------------------------------------------------------------------------------------------
// Instances

/// A loaded plugin, in its process: its GUI-thread half, and the `local` its sound thread runs.
struct Loaded {
    info: Info,
    local: Arc<Mutex<Local>>,
    /// Why its window didn't open, until it's asked again.
    editor_error: Option<String>,
    plug: Plug,
    editor: Option<Editor>,
    // Last: unloaded after everything above is gone.
    _lib: Arc<Lib>,
}

enum Plug {
    Vst3 {
        comp: ComPtr<IComponent>,
        ctrl: ComPtr<IEditController>,
        /// The controller is its own object (not the component itself).
        separate: bool,
        _handler: ComWrapper<Handler>,
        _host: ComWrapper<HostApp>,
        _factory: ComPtr<IPluginFactory>,
    },
    Clap {
        plugin: *const clap_plugin,
        host: Box<ClapHost>,
    },
}

impl Loaded {
    /// Loads and switches on a plugin, with its saved settings (if any). GUI thread.
    fn load(info: &Info, state: &[u8]) -> Result<Loaded, String> {
        let lib = Lib::open(&info.path, info.format)?;
        // SAFETY: the library is loaded and matches the format; everything runs on the GUI
        // thread, which both formats require for these calls.
        let (plug, run, ins, outs) = unsafe {
            match info.format {
                Format::Vst3 => load_vst3(&lib, info, state)?,
                Format::Clap => load_clap(&lib, info, state)?,
            }
        };
        let (main_in, main_out) = (ins.first().copied().unwrap_or(0), outs[0]);
        let ((ins, in_ptrs, in_starts), (outs, out_ptrs, out_starts)) = (buffers(&ins), buffers(&outs));
        let mut proc = Local { run, ins, outs, _in_ptrs: in_ptrs, _out_ptrs: out_ptrs, main_in, main_out, started: false, stopped: false, _lib: lib.clone() };
        // Point the bus structs at the channels.
        let (ip, op) = (proc._in_ptrs.as_mut_ptr(), proc._out_ptrs.as_mut_ptr());
        match &mut proc.run {
            Run::Vst3 { in_bus, out_bus, .. } => {
                for (b, &s) in in_bus.iter_mut().zip(&in_starts) {
                    b.__field0.channelBuffers32 = unsafe { ip.add(s) };
                }
                for (b, &s) in out_bus.iter_mut().zip(&out_starts) {
                    b.__field0.channelBuffers32 = unsafe { op.add(s) };
                }
            }
            Run::Clap { in_bus, out_bus, .. } => {
                for (b, &s) in in_bus.iter_mut().zip(&in_starts) {
                    b.data32 = unsafe { ip.add(s) };
                }
                for (b, &s) in out_bus.iter_mut().zip(&out_starts) {
                    b.data32 = unsafe { op.add(s) };
                }
            }
        }
        Ok(Loaded { info: info.clone(), local: Arc::new(Mutex::new(proc)), editor_error: None, plug, editor: None, _lib: lib })
    }

    /// The plugin's settings, to save. GUI thread.
    fn state(&self) -> Vec<u8> {
        // SAFETY: GUI-thread calls on a live, initialised plugin.
        unsafe {
            match &self.plug {
                Plug::Vst3 { comp, ctrl, .. } => {
                    let (a, b) = (Stream::new(&[]), Stream::new(&[]));
                    comp.getState(stream_ptr(&a));
                    ctrl.getState(stream_ptr(&b));
                    join_state(&a.data.borrow(), &b.data.borrow())
                }
                Plug::Clap { plugin, .. } => {
                    let mut v = Vec::new();
                    if let Some(s) = ext::<clap_plugin_state>(*plugin, CLAP_EXT_STATE).and_then(|s| s.save) {
                        let os = clap_ostream { ctx: &mut v as *mut Vec<u8> as *mut c_void, write: Some(write_bytes) };
                        if !s(*plugin, &os) {
                            v.clear();
                        }
                    }
                    v
                }
            }
        }
    }

    fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// Opens the plugin's own window at `dpi` (or brings it forward). Its own window, not one
    /// owned by MicClean's: that would tie the two processes' input together, and a hung plugin
    /// would hang MicClean's window with it.
    fn open_editor(&mut self, dpi: u32, dark: bool) {
        if let Some(e) = &self.editor {
            unsafe { wm::SetForegroundWindow(e.hwnd) };
            return;
        }
        let title = format!("{} · MicClean", self.info.name);
        // SAFETY: GUI-thread calls on a live plugin; the window outlives the view (closed first).
        let r = unsafe {
            match &self.plug {
                Plug::Vst3 { ctrl, .. } => open_vst3_editor(ctrl, &title, dpi),
                Plug::Clap { plugin, host } => open_clap_editor(*plugin, host, &title, dpi),
            }
        };
        match r {
            Ok(e) => {
                crate::ui::title_bar(e.hwnd as isize, dark);
                unsafe {
                    wm::ShowWindow(e.hwnd, wm::SW_SHOW);
                    wm::SetForegroundWindow(e.hwnd);
                }
                self.editor = Some(e);
                self.editor_error = None;
            }
            Err(why) => self.editor_error = Some(why),
        }
    }

    fn close_editor(&mut self) {
        let Some(e) = self.editor.take() else { return };
        // SAFETY: the view is attached to this window; it's taken off before the window goes.
        unsafe {
            match (&e.view, &self.plug) {
                (View::Vst3 { view, .. }, _) => {
                    view.removed();
                    view.setFrame(null_mut());
                }
                (View::Clap, Plug::Clap { plugin, host }) => {
                    if let Some(g) = ext::<clap_plugin_gui>(*plugin, CLAP_EXT_GUI) {
                        g.hide.map(|f| f(*plugin));
                        g.destroy.map(|f| f(*plugin));
                    }
                    host.hwnd.store(0, Relaxed);
                }
                _ => {}
            }
            wm::SetWindowLongPtrW(e.hwnd, wm::GWLP_USERDATA, 0);
            wm::DestroyWindow(e.hwnd);
        }
    }

    /// GUI-thread chores, every frame: callbacks a CLAP plugin asked for, its window closing
    /// or being resized.
    fn idle(&mut self) {
        if let Plug::Clap { plugin, host } = &self.plug {
            let plugin = *plugin;
            // SAFETY: GUI-thread calls the plugin asked for, on a live plugin.
            unsafe {
                if host.callback.swap(false, Relaxed)
                    && let Some(f) = (*plugin).on_main_thread
                {
                    f(plugin);
                }
                if host.restart.swap(false, Relaxed) {
                    // ponytail: restarts while the sound thread waits (MicClean gets those frames dry).
                    let mut p = self.local.lock().unwrap_or_else(|e| e.into_inner());
                    if p.started {
                        (*plugin).stop_processing.map(|f| f(plugin));
                        p.started = false;
                    }
                    (*plugin).deactivate.map(|f| f(plugin));
                    if !(*plugin).activate.is_some_and(|f| f(plugin, SR as f64, 1, FRAME as u32)) {
                        p.stopped = true;
                    }
                }
            }
        }
        let Some(e) = &self.editor else { return };
        if e.win.close.get() || unsafe { wm::IsWindow(e.hwnd) } == 0 {
            self.close_editor();
            return;
        }
        if let Some((w, h)) = e.win.pending.take() {
            e.win.last.set((w, h));
            // SAFETY: GUI-thread calls on the attached view.
            unsafe {
                match (&e.view, &self.plug) {
                    (View::Vst3 { view, .. }, _) => {
                        let mut r = ViewRect { left: 0, top: 0, right: w, bottom: h };
                        view.onSize(&mut r);
                    }
                    (View::Clap, Plug::Clap { plugin, .. }) => {
                        if let Some(g) = ext::<clap_plugin_gui>(*plugin, CLAP_EXT_GUI) {
                            let (mut w, mut h) = (w as u32, h as u32);
                            g.adjust_size.map(|f| f(*plugin, &mut w, &mut h));
                            g.set_size.map(|f| f(*plugin, w, h));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Drop for Loaded {
    fn drop(&mut self) {
        self.close_editor();
        let mut proc = self.local.lock().unwrap_or_else(|e| e.into_inner());
        proc.stop();
        // SAFETY: GUI-thread shutdown in the order each format asks for; nothing processes now.
        unsafe {
            match &self.plug {
                Plug::Vst3 { comp, ctrl, separate, .. } => {
                    comp.setActive(0);
                    if *separate && let (Some(a), Some(b)) = (comp.cast::<IConnectionPoint>(), ctrl.cast::<IConnectionPoint>()) {
                        a.disconnect(b.as_ptr());
                        b.disconnect(a.as_ptr());
                    }
                    ctrl.setComponentHandler(null_mut());
                    if *separate {
                        ctrl.terminate();
                    }
                    comp.terminate();
                }
                Plug::Clap { plugin, .. } => {
                    (**plugin).deactivate.map(|f| f(*plugin));
                    (**plugin).destroy.map(|f| f(*plugin));
                }
            }
        }
    }
}

/// # Safety
/// GUI thread; `lib` is a loaded VST3 module.
unsafe fn load_vst3(lib: &Arc<Lib>, info: &Info, state: &[u8]) -> Result<(Plug, Run, Vec<usize>, Vec<usize>), String> {
    let factory = unsafe { vst3_factory(lib)? };
    let cid = parse_uid(&info.uid)?;
    let host = ComWrapper::new(HostApp);
    let host_ptr = host.as_com_ref::<FUnknown>().expect("every class is an FUnknown").as_ptr();
    let comp: ComPtr<IComponent> = unsafe { create(&factory, &cid) }.ok_or("The plugin couldn't be made")?;
    if unsafe { comp.initialize(host_ptr) } != kResultOk {
        return Err("The plugin didn't start".into());
    }
    let (ctrl, separate) = match comp.cast::<IEditController>() {
        Some(c) => (c, false),
        None => {
            let mut ccid: TUID = [0; 16];
            if unsafe { comp.getControllerClassId(&mut ccid) } != kResultOk {
                return Err("The plugin has no controls".into());
            }
            let c: ComPtr<IEditController> = unsafe { create(&factory, &ccid) }.ok_or("The plugin's controls couldn't be made")?;
            if unsafe { c.initialize(host_ptr) } != kResultOk {
                return Err("The plugin's controls didn't start".into());
            }
            (c, true)
        }
    };
    if separate && let (Some(a), Some(b)) = (comp.cast::<IConnectionPoint>(), ctrl.cast::<IConnectionPoint>()) {
        unsafe {
            a.connect(b.as_ptr());
            b.connect(a.as_ptr());
        }
    }
    let edits = Arc::new(Mutex::new(Vec::new()));
    let handler = ComWrapper::new(Handler { edits: edits.clone() });
    unsafe { ctrl.setComponentHandler(handler.as_com_ref::<IComponentHandler>().expect("listed").as_ptr()) };

    // Saved settings (else the plugin's own defaults) go to the controller too.
    let (cs, ks) = split_state(state);
    unsafe {
        if !cs.is_empty() {
            comp.setState(stream_ptr(&Stream::new(cs)));
        }
        let s = Stream::new(&[]);
        comp.getState(stream_ptr(&s));
        s.pos.set(0);
        ctrl.setComponentState(stream_ptr(&s));
        if !ks.is_empty() {
            ctrl.setState(stream_ptr(&Stream::new(ks)));
        }
    }

    let proc = comp.cast::<IAudioProcessor>().ok_or("The plugin can't process sound")?;
    let (audio, input, output) = (MediaTypes_::kAudio as MediaType, BusDirections_::kInput as BusDirection, BusDirections_::kOutput as BusDirection);
    let count = |dir| unsafe { comp.getBusCount(audio, dir) }.max(0) as usize;
    let (n_in, n_out) = (count(input), count(output));
    if n_out == 0 {
        return Err("The plugin has no audio output".into());
    }
    let arr = |dir, i: usize| {
        let mut a: SpeakerArrangement = 0;
        unsafe { proc.getBusArrangement(dir, i as int32, &mut a) };
        a
    };
    let mut ins: Vec<SpeakerArrangement> = (0..n_in).map(|i| arr(input, i)).collect();
    let mut outs: Vec<SpeakerArrangement> = (0..n_out).map(|i| arr(output, i)).collect();
    // Mono if it takes mono, else stereo; else whatever it has.
    for want in [SpeakerArr::kMono, SpeakerArr::kStereo] {
        if let Some(a) = ins.first_mut() {
            *a = want;
        }
        outs[0] = want;
        if unsafe { proc.setBusArrangements(ins.as_mut_ptr(), n_in as int32, outs.as_mut_ptr(), n_out as int32) } == kResultTrue {
            break;
        }
    }
    let ins: Vec<usize> = (0..n_in).map(|i| arr(input, i).count_ones() as usize).collect();
    let outs: Vec<usize> = (0..n_out).map(|i| arr(output, i).count_ones() as usize).collect();
    if outs[0] == 0 {
        return Err("The plugin has no audio output".into());
    }
    unsafe {
        if n_in > 0 {
            comp.activateBus(audio, input, 0, 1);
        }
        comp.activateBus(audio, output, 0, 1);
    }
    let mut setup = ProcessSetup {
        processMode: ProcessModes_::kRealtime as int32,
        symbolicSampleSize: SymbolicSampleSizes_::kSample32 as int32,
        maxSamplesPerBlock: FRAME as int32,
        sampleRate: SR as f64,
    };
    if unsafe { proc.setupProcessing(&mut setup) } != kResultOk {
        return Err("The plugin can't run at 48 kHz".into());
    }
    if unsafe { comp.setActive(1) } != kResultOk {
        return Err("The plugin didn't switch on".into());
    }
    let bus = |n: usize| AudioBusBuffers { numChannels: n as int32, silenceFlags: 0, __field0: AudioBusBuffers__type0 { channelBuffers32: null_mut() } };
    let mut ctx: ProcessContext = unsafe { std::mem::zeroed() };
    (ctx.sampleRate, ctx.tempo, ctx.timeSigNumerator, ctx.timeSigDenominator) = (SR as f64, 120.0, 4, 4);
    ctx.state = (ProcessContext_::StatesAndFlags_::kTempoValid | ProcessContext_::StatesAndFlags_::kTimeSigValid) as u32;
    let run = Run::Vst3 {
        proc,
        in_bus: ins.iter().map(|&n| bus(n)).collect(),
        out_bus: outs.iter().map(|&n| bus(n)).collect(),
        changes: Changes::new(),
        out_changes: Changes::new(),
        edits,
        ctx,
    };
    let plug = Plug::Vst3 { comp, ctrl, separate, _handler: handler, _host: host, _factory: factory };
    Ok((plug, run, ins, outs))
}

/// # Safety
/// GUI thread; `lib` is a loaded, initialised CLAP library.
unsafe fn load_clap(lib: &Arc<Lib>, info: &Info, state: &[u8]) -> Result<(Plug, Run, Vec<usize>, Vec<usize>), String> {
    let factory = unsafe { clap_factory(lib)? };
    let host = ClapHost::new();
    let id = CString::new(info.uid.clone()).map_err(|_| "Bad plugin ID")?;
    let create = factory.create_plugin.ok_or("The plugin couldn't be made")?;
    let plugin = unsafe { create(factory, &host.host, id.as_ptr()) };
    if plugin.is_null() {
        return Err("The plugin couldn't be made".into());
    }
    let fail = |why: &str| {
        unsafe { (*plugin).destroy.map(|f| f(plugin)) };
        Err(why.to_string())
    };
    unsafe {
        if !(*plugin).init.is_some_and(|f| f(plugin)) {
            return fail("The plugin didn't start");
        }
        if !state.is_empty()
            && let Some(load) = ext::<clap_plugin_state>(plugin, CLAP_EXT_STATE).and_then(|s| s.load)
        {
            let mut r = (state, 0usize);
            let is = clap_istream { ctx: &mut r as *mut (&[u8], usize) as *mut c_void, read: Some(read_bytes) };
            load(plugin, &is);
        }
    }
    let ports = unsafe { ext::<clap_plugin_audio_ports>(plugin, CLAP_EXT_AUDIO_PORTS) };
    let counts = |is_input: bool| -> Vec<usize> {
        let Some(a) = ports else { return Vec::new() };
        let n = a.count.map_or(0, |c| unsafe { c(plugin, is_input) });
        (0..n)
            .map(|i| {
                let mut p: clap_audio_port_info = unsafe { std::mem::zeroed() };
                a.get.map(|g| unsafe { g(plugin, i, is_input, &mut p) });
                p.channel_count as usize
            })
            .collect()
    };
    let (ins, outs) = (counts(true), counts(false));
    if outs.first().is_none_or(|&n| n == 0) {
        return fail("The plugin has no audio output");
    }
    if !unsafe { (*plugin).activate.is_some_and(|f| f(plugin, SR as f64, 1, FRAME as u32)) } {
        return fail("The plugin didn't switch on");
    }
    let bus = |n: usize| clap_audio_buffer { data32: null_mut(), data64: null_mut(), channel_count: n as u32, latency: 0, constant_mask: 0 };
    let run = Run::Clap { plugin, in_bus: ins.iter().map(|&n| bus(n)).collect(), out_bus: outs.iter().map(|&n| bus(n)).collect(), steady: 0 };
    Ok((Plug::Clap { plugin, host }, run, ins, outs))
}

// ---------------------------------------------------------------------------------------------
// Plugin windows

struct Editor {
    hwnd: HWND,
    win: Rc<WinState>,
    view: View,
}

enum View {
    Vst3 { view: ComPtr<IPlugView>, _frame: ComWrapper<Frame> },
    Clap,
}

/// What the window procedure tells the GUI thread: closed, or resized by the user.
#[derive(Default)]
struct WinState {
    close: Cell<bool>,
    pending: Cell<Option<(i32, i32)>>,
    /// The plugin's size as last set, so a resize it asked for isn't passed back to it.
    last: Cell<(i32, i32)>,
}

const CLASS: &str = "MicCleanPlugin";

/// # Safety
/// `hwnd` is one of our plugin windows, or any window (then None).
unsafe fn win_state<'a>(hwnd: HWND) -> Option<&'a WinState> {
    unsafe { (wm::GetWindowLongPtrW(hwnd, wm::GWLP_USERDATA) as *const WinState).as_ref() }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        // Closed on the next GUI frame, view first.
        wm::WM_CLOSE => {
            if let Some(w) = unsafe { win_state(hwnd) } {
                w.close.set(true);
            }
            return 0;
        }
        wm::WM_SIZE if wp != wm::SIZE_MINIMIZED as usize => {
            let size = ((lp & 0xFFFF) as i32, ((lp >> 16) & 0xFFFF) as i32);
            if let Some(w) = unsafe { win_state(hwnd) }
                && w.last.get() != size
            {
                w.pending.set(Some(size));
            }
        }
        wm::WM_ERASEBKGND => return 1,
        _ => {}
    }
    unsafe { wm::DefWindowProcW(hwnd, msg, wp, lp) }
}

fn style(resizable: bool) -> u32 {
    let base = wm::WS_OVERLAPPED | wm::WS_CAPTION | wm::WS_SYSMENU | wm::WS_MINIMIZEBOX | wm::WS_CLIPCHILDREN;
    if resizable { base | wm::WS_THICKFRAME | wm::WS_MAXIMIZEBOX } else { base }
}

fn window_size(style: u32, dpi: u32, w: i32, h: i32) -> (i32, i32) {
    let mut r = RECT { left: 0, top: 0, right: w, bottom: h };
    unsafe { AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi) };
    (r.right - r.left, r.bottom - r.top)
}

fn set_client_size(hwnd: HWND, w: i32, h: i32) {
    if hwnd.is_null() {
        return;
    }
    unsafe {
        let style = wm::GetWindowLongW(hwnd, wm::GWL_STYLE) as u32;
        let (ww, wh) = window_size(style, GetDpiForWindow(hwnd), w, h);
        wm::SetWindowPos(hwnd, null_mut(), 0, 0, ww, wh, wm::SWP_NOMOVE | wm::SWP_NOZORDER | wm::SWP_NOACTIVATE);
    }
}

/// A hidden window with room for a w x h plugin view.
fn make_window(title: &str, w: i32, h: i32, resizable: bool, dpi: u32, win: &Rc<WinState>) -> Result<HWND, String> {
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    static REGISTERED: OnceLock<bool> = OnceLock::new();
    let class: Vec<u16> = CLASS.encode_utf16().chain([0]).collect();
    // SAFETY: plain Win32 window calls; the class name and title outlive them, and the state
    // pointer stored in the window is cleared before `win` can go.
    unsafe {
        let inst = GetModuleHandleW(null());
        REGISTERED.get_or_init(|| {
            let wc = wm::WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: inst,
                hIcon: null_mut(),
                hCursor: wm::LoadCursorW(null_mut(), wm::IDC_ARROW),
                hbrBackground: null_mut(),
                lpszMenuName: null(),
                lpszClassName: class.as_ptr(),
            };
            wm::RegisterClassW(&wc) != 0
        });
        let st = style(resizable);
        let (ww, wh) = window_size(st, dpi, w, h);
        let title: Vec<u16> = title.encode_utf16().chain([0]).collect();
        let hwnd = wm::CreateWindowExW(0, class.as_ptr(), title.as_ptr(), st, wm::CW_USEDEFAULT, wm::CW_USEDEFAULT, ww, wh, null_mut(), null_mut(), inst, null());
        if hwnd.is_null() {
            return Err(format!("Windows couldn't make the window ({})", std::io::Error::last_os_error()));
        }
        win.last.set((w, h));
        wm::SetWindowLongPtrW(hwnd, wm::GWLP_USERDATA, Rc::as_ptr(win) as isize);
        Ok(hwnd)
    }
}

/// # Safety
/// GUI thread; `ctrl` is a live edit controller.
unsafe fn open_vst3_editor(ctrl: &ComPtr<IEditController>, title: &str, dpi: u32) -> Result<Editor, String> {
    let view = unsafe { ComPtr::from_raw(ctrl.createView(ViewType::kEditor)) }.ok_or("This plugin has no window")?;
    if unsafe { view.isPlatformTypeSupported(kPlatformTypeHWND) } != kResultTrue {
        return Err("This plugin's window doesn't work on Windows".into());
    }
    let win = Rc::new(WinState::default());
    let frame = ComWrapper::new(Frame { hwnd: Cell::new(0), win: win.clone() });
    unsafe { view.setFrame(frame.as_com_ref::<IPlugFrame>().expect("listed").as_ptr()) };
    if let Some(s) = view.cast::<IPlugViewContentScaleSupport>() {
        unsafe { s.setContentScaleFactor(dpi as f32 / 96.0) };
    }
    let mut r = ViewRect { left: 0, top: 0, right: 0, bottom: 0 };
    unsafe { view.getSize(&mut r) };
    let (w, h) = ((r.right - r.left).max(100), (r.bottom - r.top).max(60));
    let resizable = unsafe { view.canResize() } == kResultTrue;
    let hwnd = make_window(title, w, h, resizable, dpi, &win)?;
    frame.hwnd.set(hwnd as isize);
    if unsafe { view.attached(hwnd as *mut c_void, kPlatformTypeHWND) } != kResultOk {
        unsafe {
            view.setFrame(null_mut());
            wm::DestroyWindow(hwnd);
        }
        return Err("The plugin's window didn't open".into());
    }
    Ok(Editor { hwnd, win, view: View::Vst3 { view, _frame: frame } })
}

/// # Safety
/// GUI thread; `plugin` is a live, initialised CLAP plugin made with `host`.
unsafe fn open_clap_editor(plugin: *const clap_plugin, host: &ClapHost, title: &str, dpi: u32) -> Result<Editor, String> {
    let g = unsafe { ext::<clap_plugin_gui>(plugin, CLAP_EXT_GUI) }.ok_or("This plugin has no window")?;
    let api = CLAP_WINDOW_API_WIN32.as_ptr();
    unsafe {
        if !g.is_api_supported.is_some_and(|f| f(plugin, api, false)) || !g.create.is_some_and(|f| f(plugin, api, false)) {
            return Err("This plugin's window doesn't work on Windows".into());
        }
        g.set_scale.map(|f| f(plugin, dpi as f64 / 96.0));
        let (mut w, mut h) = (0u32, 0u32);
        g.get_size.map(|f| f(plugin, &mut w, &mut h));
        let resizable = g.can_resize.is_some_and(|f| f(plugin));
        let win = Rc::new(WinState::default());
        let hwnd = match make_window(title, (w as i32).max(100), (h as i32).max(60), resizable, dpi, &win) {
            Ok(h) => h,
            Err(e) => {
                g.destroy.map(|f| f(plugin));
                return Err(e);
            }
        };
        let parent = clap_window { api, specific: clap_window_handle { win32: hwnd } };
        if !g.set_parent.is_some_and(|f| f(plugin, &parent)) {
            g.destroy.map(|f| f(plugin));
            wm::DestroyWindow(hwnd);
            return Err("The plugin's window didn't open".into());
        }
        host.hwnd.store(hwnd as isize, Relaxed);
        g.show.map(|f| f(plugin));
        Ok(Editor { hwnd, win, view: View::Clap })
    }
}

// ---------------------------------------------------------------------------------------------
// Each plugin in its own process
//
// MicClean starts `micclean --plugin-host <name>` for each plugin in the rack. That process
// loads the plugin and runs it as above (its main thread is the plugin's GUI thread, a second
// thread its sound). MicClean talks to it in JSON lines on its stdin and stdout, and hands it
// the sound one frame at a time through shared memory `name` and two events. A plugin that
// crashes or hangs takes only its own process down: MicClean's audio goes on without it.

/// What MicClean asks of a plugin process.
#[derive(Serialize, Deserialize)]
enum Cmd {
    /// The first line: which plugin, with its saved settings (hex).
    Load { info: Info, state: String },
    State,
    /// Open its window at this DPI (MicClean's window's), dark or light.
    OpenEditor { dpi: u32, dark: bool },
    CloseEditor,
    Quit,
}

/// What a plugin process tells MicClean.
#[derive(Serialize, Deserialize)]
enum Msg {
    Ready,
    Failed(String),
    State(String),
    /// Its window is open (or not), and why it didn't open.
    Editor { open: bool, error: Option<String> },
}

/// The memory a plugin's sound goes through. MicClean writes a frame and its number in
/// `sent`; the plugin process works on it in place and puts the number in `done`. The buffer
/// belongs to whoever's turn it is: MicClean's while `done == sent`, the plugin's otherwise.
#[repr(C)]
struct Shm {
    sent: AtomicU32,
    done: AtomicU32,
    buf: UnsafeCell<[f32; FRAME]>,
}

/// One plugin process's shared memory and its two events ("go": a frame is waiting; "done":
/// it's back), by name.
struct Link {
    map: HANDLE,
    shm: *mut Shm,
    go: HANDLE,
    done: HANDLE,
}

// SAFETY: handles and a mapping, usable from any thread; who touches the buffer when is
// settled by the frame numbers (see Shm).
unsafe impl Send for Link {}

impl Link {
    /// MicClean's side: makes them.
    fn create(name: &str) -> Result<Link, String> {
        Link::make(name, true)
    }

    /// The plugin process's side: opens MicClean's.
    fn open(name: &str) -> Result<Link, String> {
        Link::make(name, false)
    }

    fn make(name: &str, create: bool) -> Result<Link, String> {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Memory::{CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, OpenFileMappingW, PAGE_READWRITE};
        use windows_sys::Win32::System::Threading::{CreateEventW, EVENT_ALL_ACCESS, OpenEventW};
        let w = |s: &str| -> Vec<u16> { format!("Local\\{name}-{s}").encode_utf16().chain([0]).collect() };
        let err = |what: &str| format!("{what} ({})", std::io::Error::last_os_error());
        let size = std::mem::size_of::<Shm>();
        let mut link = Link { map: null_mut(), shm: null_mut(), go: null_mut(), done: null_mut() };
        // SAFETY: plain Win32 calls with NUL-terminated names that outlive them. Whatever was
        // made before an error is closed when `link` drops.
        unsafe {
            link.map = if create {
                CreateFileMappingW(INVALID_HANDLE_VALUE, null(), PAGE_READWRITE, 0, size as u32, w("mem").as_ptr())
            } else {
                OpenFileMappingW(FILE_MAP_ALL_ACCESS, 0, w("mem").as_ptr())
            };
            if link.map.is_null() {
                return Err(err("Couldn't share memory with the plugin"));
            }
            link.shm = MapViewOfFile(link.map, FILE_MAP_ALL_ACCESS, 0, 0, size).Value as *mut Shm;
            if link.shm.is_null() {
                return Err(err("Couldn't share memory with the plugin"));
            }
            for (h, s) in [(&mut link.go, "go"), (&mut link.done, "done")] {
                *h = if create { CreateEventW(null(), 0, 0, w(s).as_ptr()) } else { OpenEventW(EVENT_ALL_ACCESS, 0, w(s).as_ptr()) };
                if h.is_null() {
                    return Err(err("Couldn't set up the plugin's signals"));
                }
            }
        }
        Ok(link)
    }

    fn shm(&self) -> &Shm {
        // SAFETY: mapped (and zeroed when made) for as long as the Link lives.
        unsafe { &*self.shm }
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};
        // SAFETY: only what make() opened, once.
        unsafe {
            if !self.shm.is_null() {
                UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS { Value: self.shm as *mut c_void });
            }
            for h in [self.map, self.go, self.done] {
                if !h.is_null() {
                    CloseHandle(h);
                }
            }
        }
    }
}

/// How long the audio waits for a plugin process to hand a frame back before it goes on dry.
/// ponytail: a wait rounds up to Windows' timer (up to 15.6 ms) unless something asked for a
/// finer one, but only a plugin that's already late waits at all, and only once.
const WAIT_MS: u32 = 5;
/// Frames in a row a plugin process hasn't handed back before it counts as hung (2 s).
const HUNG_FRAMES: u32 = 200;

/// A plugin process's health as the audio thread sees it.
#[derive(Default)]
struct Health {
    dead: AtomicBool,
    late: AtomicU32,
}

/// What the audio thread runs for one plugin: hands each frame to its process and takes it
/// back. It never waits longer than WAIT_MS.
pub struct Proc {
    link: Link,
    /// The process, to tell a crash from a slow frame.
    process: HANDLE,
    sent: u32,
    health: Arc<Health>,
}

// SAFETY: see Link; the process handle is only waited on.
unsafe impl Send for Proc {}

impl Proc {
    /// Runs one block of mono sound through the plugin, in place. A block that doesn't come
    /// back in time (late, hung, crashed) stays dry.
    pub fn process(&mut self, x: &mut [f32]) {
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::System::Threading::{SetEvent, WaitForSingleObject};
        if self.health.dead.load(Relaxed) {
            return;
        }
        let n = x.len().min(FRAME);
        let shm = self.link.shm();
        // Still on an earlier frame (it came back late): the buffer is the plugin's until then.
        if shm.done.load(Acquire) == self.sent {
            self.sent = self.sent.wrapping_add(1);
            // SAFETY: done == sent: the plugin process is between frames, the buffer is ours.
            unsafe { (&mut *shm.buf.get())[..n].copy_from_slice(&x[..n]) };
            shm.sent.store(self.sent, Release);
            unsafe { SetEvent(self.link.go) };
            let start = Instant::now();
            loop {
                if shm.done.load(Acquire) == self.sent {
                    // SAFETY: done == sent again: it's finished with the buffer.
                    x[..n].copy_from_slice(unsafe { &(&*shm.buf.get())[..n] });
                    self.health.late.store(0, Relaxed);
                    return;
                }
                let left = WAIT_MS.saturating_sub(start.elapsed().as_millis() as u32);
                if left == 0 {
                    break;
                }
                // A "done" left over from a frame that came back late can wake this early;
                // the frame number above decides.
                unsafe { WaitForSingleObject(self.link.done, left) };
            }
        }
        self.health.late.fetch_add(1, Relaxed);
        if unsafe { WaitForSingleObject(self.process, 0) } == WAIT_OBJECT_0 {
            self.health.dead.store(true, Relaxed);
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        // SAFETY: our own handle from OpenProcess (or null).
        unsafe { windows_sys::Win32::Foundation::CloseHandle(self.process) };
    }
}

/// The plugin process's sound thread: each frame MicClean hands over goes through the
/// plugin and back, until `quit`.
fn serve(link: &Link, local: &Mutex<Local>, quit: &AtomicBool) {
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{SetEvent, WaitForSingleObject};
    let shm = link.shm();
    while !quit.load(Relaxed) {
        if unsafe { WaitForSingleObject(link.go, 100) } != WAIT_OBJECT_0 {
            continue;
        }
        let n = shm.sent.load(Acquire);
        if n == shm.done.load(Relaxed) {
            continue;
        }
        // SAFETY: sent != done: MicClean handed this frame over and leaves it alone until done.
        let buf = unsafe { &mut *shm.buf.get() };
        local.lock().unwrap_or_else(|e| e.into_inner()).process(buf);
        shm.done.store(n, Release);
        unsafe { SetEvent(link.done) };
    }
}

/// One job for every plugin process, which Windows closes with MicClean: none is left running
/// after MicClean ends, however it ends.
fn job() -> HANDLE {
    use windows_sys::Win32::System::JobObjects::{
        CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation, SetInformationJobObject,
    };
    static JOB: OnceLock<usize> = OnceLock::new();
    // SAFETY: a fresh job object and a zeroed limits struct with one flag set.
    *JOB.get_or_init(|| unsafe {
        let j = CreateJobObjectW(null(), null());
        if !j.is_null() {
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(j, JobObjectExtendedLimitInformation, &info as *const _ as *const c_void, std::mem::size_of_val(&info) as u32);
        }
        j as usize
    }) as HANDLE
}

/// What happened to a plugin process since the last look.
enum Status {
    Running,
    /// Loaded: its sound can go to the audio thread.
    Ready,
    /// It couldn't load the plugin, and said why.
    Failed(String),
    /// It crashed or hung (and was ended).
    Died(String),
}

/// A plugin running in its own process, as MicClean's GUI sees it.
pub struct Instance {
    pub info: Info,
    /// Why its window didn't open, until it's asked again.
    pub editor_error: Option<String>,
    proc: Arc<Mutex<Proc>>,
    health: Arc<Health>,
    stdin: Option<std::process::ChildStdin>,
    child: std::process::Child,
    msgs: std::sync::mpsc::Receiver<Msg>,
    ready: bool,
    editor: bool,
    /// Its settings as it last reported them (to start with, as it was loaded with).
    state: Vec<u8>,
}

impl Instance {
    /// Starts its process (`exe --plugin-host`) and asks it to load the plugin. Returns at
    /// once; `poll` says when it's ready.
    fn spawn(exe: &Path, key: u32, info: &Info, state: &[u8]) -> Result<Instance, String> {
        use std::io::BufRead;
        use std::os::windows::io::AsRawHandle;
        use std::os::windows::process::CommandExt;
        use std::process::{Command, Stdio};
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE};
        static N: AtomicU32 = AtomicU32::new(0);
        let name = format!("MicClean-{}-{key}-{}", std::process::id(), N.fetch_add(1, Relaxed));
        let link = Link::create(&name)?;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut child = Command::new(exe)
            .arg("--plugin-host")
            .arg(&name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map_err(|e| format!("Its process didn't start ({e})"))?;
        // SAFETY: the job and the child's handle are live; OpenProcess makes our own handle.
        let process = unsafe {
            windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job(), child.as_raw_handle() as HANDLE);
            OpenProcess(PROCESS_SYNCHRONIZE, 0, child.id())
        };
        let stdout = child.stdout.take().expect("piped above");
        let (tx, msgs) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Ok(m) = serde_json::from_str(&line)
                    && tx.send(m).is_err()
                {
                    return;
                }
            }
        });
        let health = Arc::new(Health::default());
        let proc = Arc::new(Mutex::new(Proc { link, process, sent: 0, health: health.clone() }));
        let stdin = child.stdin.take();
        let mut inst = Instance { info: info.clone(), editor_error: None, proc, health, stdin, child, msgs, ready: false, editor: false, state: state.to_vec() };
        inst.send(&Cmd::Load { info: info.clone(), state: to_hex(state) });
        Ok(inst)
    }

    fn send(&mut self, c: &Cmd) -> bool {
        use std::io::Write;
        let Some(s) = &mut self.stdin else { return false };
        serde_json::to_string(c).is_ok_and(|l| writeln!(s, "{l}").and_then(|()| s.flush()).is_ok())
    }

    /// What the process said since the last look, and whether it's alive and keeping up.
    fn poll(&mut self) -> Status {
        let mut status = Status::Running;
        while let Ok(m) = self.msgs.try_recv() {
            match m {
                Msg::Ready => {
                    self.ready = true;
                    status = Status::Ready;
                }
                Msg::Failed(why) => return Status::Failed(why),
                Msg::State(h) => self.state = from_hex(&h),
                Msg::Editor { open, error } => (self.editor, self.editor_error) = (open, error),
            }
        }
        if let Ok(Some(end)) = self.child.try_wait() {
            // Its last words may still be on their way: "couldn't load" isn't a crash.
            while let Ok(m) = self.msgs.recv_timeout(Duration::from_millis(200)) {
                if let Msg::Failed(why) = m {
                    return Status::Failed(why);
                }
            }
            return Status::Died(match end.code() {
                Some(0) => "it closed by itself".into(),
                Some(c) => format!("it crashed (error {:#010x})", c as u32),
                None => "it crashed".into(),
            });
        }
        if self.health.late.load(Relaxed) >= HUNG_FRAMES {
            let _ = self.child.kill();
            return Status::Died("it stopped responding".into());
        }
        status
    }

    pub fn editor_open(&self) -> bool {
        self.editor
    }

    /// Opens the plugin's window at the DPI of MicClean's (or brings it forward).
    pub fn open_editor(&mut self, owner: Option<isize>, dark: bool) {
        let dpi = owner.map_or(96, |o| unsafe { GetDpiForWindow(o as HWND) }).max(96);
        // The click that asked for it was ours, so its window may come to the front.
        unsafe { wm::AllowSetForegroundWindow(self.child.id()) };
        self.editor_error = None;
        self.send(&Cmd::OpenEditor { dpi, dark });
    }

    pub fn close_editor(&mut self) {
        self.send(&Cmd::CloseEditor);
    }

    /// Its settings now (asked for, waiting up to a second), or as it last said.
    fn state(&mut self) -> Vec<u8> {
        if self.ready && self.send(&Cmd::State) {
            let end = Instant::now() + Duration::from_secs(1);
            while let Ok(m) = self.msgs.recv_timeout(end.saturating_duration_since(Instant::now())) {
                match m {
                    Msg::State(h) => {
                        self.state = from_hex(&h);
                        break;
                    }
                    Msg::Editor { open, error } => (self.editor, self.editor_error) = (open, error),
                    Msg::Ready | Msg::Failed(_) => {}
                }
            }
        }
        self.state.clone()
    }

    /// Asks the process to unload the plugin and end (closing its input says the same).
    fn quit(&mut self) {
        self.send(&Cmd::Quit);
        self.stdin = None;
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.quit();
        // 2 s to shut the plugin down, then its process is ended.
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end && matches!(self.child.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The plugins in the rack, each in its own process (or why it couldn't start).
#[derive(Default)]
pub struct Host {
    loaded: Vec<(u32, Result<Instance, String>)>,
    /// Out of the rack (or dead), waiting for the audio thread to let go of their Proc.
    closing: Vec<Instance>,
    /// The audio thread needs the list again: it changed, or audio restarted.
    pub resend: bool,
    /// The exe the plugin processes run: this one, unless a test says otherwise.
    exe: Option<PathBuf>,
}

impl Host {
    /// Starts what the rack gained, stops what it lost (its settings saved first). Returns the
    /// list for the audio thread when it changed.
    pub fn sync(&mut self, wanted: &[u32], entries: &mut [Entry]) -> Option<Procs> {
        let keep = |key: u32, entries: &[Entry]| wanted.contains(&key) && entries.iter().any(|e| e.key == key && !e.crashed);
        let mut i = 0;
        while i < self.loaded.len() {
            let key = self.loaded[i].0;
            if keep(key, entries) {
                i += 1;
                continue;
            }
            if let (_, Ok(mut inst)) = self.loaded.remove(i) {
                if let Some(e) = entries.iter_mut().find(|e| e.key == key) {
                    e.state = to_hex(&inst.state());
                }
                inst.quit();
                self.closing.push(inst);
            }
            self.resend = true;
        }
        for &key in wanted {
            if self.loaded.iter().any(|(k, _)| *k == key) {
                continue;
            }
            let Some(e) = entries.iter().find(|e| e.key == key && !e.crashed) else { continue };
            let exe = self.exe.clone().or_else(|| std::env::current_exe().ok()).ok_or("MicClean can't find its own exe".to_string());
            let r = exe.and_then(|exe| Instance::spawn(&exe, key, &e.info, &from_hex(&e.state)));
            if let Err(why) = &r {
                crate::logfile::line(&format!("Plugin {} didn't load: {why}", e.info.name));
            }
            self.loaded.push((key, r));
        }
        // Dropped once the audio thread's list no longer has them.
        self.closing.retain(|i| Arc::strong_count(&i.proc) > 1);
        let ready = self.loaded.iter().filter_map(|(k, r)| Some((*k, r.as_ref().ok().filter(|i| i.ready)?.proc.clone())));
        std::mem::take(&mut self.resend).then(|| ready.collect())
    }

    /// A plugin that's running, or why it couldn't; None while its process starts.
    pub fn get(&self, key: u32) -> Option<&Result<Instance, String>> {
        self.loaded.iter().find(|(k, _)| *k == key).map(|(_, r)| r).filter(|r| r.as_ref().map_or(true, |i| i.ready))
    }

    pub fn get_mut(&mut self, key: u32) -> Option<&mut Result<Instance, String>> {
        self.loaded.iter_mut().find(|(k, _)| *k == key).map(|(_, r)| r).filter(|r| r.as_ref().map_or(true, |i| i.ready))
    }

    /// Starts it again next frame (after it couldn't load).
    pub fn retry(&mut self, key: u32) {
        self.loaded.retain(|(k, r)| *k != key || r.is_ok());
    }

    /// Hears from every plugin process, each frame. Returns the plugins whose process crashed
    /// or hung, and how: they're out of the list now (MicClean marks them crashed, which keeps
    /// them out until "Try again").
    pub fn idle(&mut self) -> Vec<(u32, String)> {
        let mut dead = Vec::new();
        for i in (0..self.loaded.len()).rev() {
            let key = self.loaded[i].0;
            let (status, name) = match &mut self.loaded[i].1 {
                Ok(inst) => (inst.poll(), inst.info.name.clone()),
                Err(_) => continue,
            };
            match status {
                Status::Running => {}
                Status::Ready => {
                    crate::logfile::line(&format!("Loaded plugin {name} in its own process"));
                    self.resend = true;
                }
                Status::Failed(why) => {
                    crate::logfile::line(&format!("Plugin {name} didn't load: {why}"));
                    if let Ok(inst) = std::mem::replace(&mut self.loaded[i].1, Err(why)) {
                        self.closing.push(inst);
                    }
                    self.resend = true;
                }
                Status::Died(why) => {
                    crate::logfile::line(&format!("Plugin {name} ended: {why}. MicClean carries on without it"));
                    if let (_, Ok(inst)) = self.loaded.remove(i) {
                        self.closing.push(inst);
                    }
                    dead.push((key, why));
                    self.resend = true;
                }
            }
        }
        dead
    }

    /// Every running plugin's settings into its entry, for saving.
    pub fn store(&mut self, entries: &mut [Entry]) {
        for (key, r) in &mut self.loaded {
            if let (Ok(inst), Some(e)) = (r, entries.iter_mut().find(|e| e.key == *key)) {
                e.state = to_hex(&inst.state());
            }
        }
    }

    /// Ends every plugin process, all at once. Only once the audio has stopped.
    pub fn shutdown(&mut self) {
        for inst in self.loaded.iter_mut().filter_map(|(_, r)| r.as_mut().ok()).chain(&mut self.closing) {
            inst.quit();
        }
        self.loaded.clear();
        self.closing.clear();
    }
}

/// `micclean --plugin-host <name>`: runs one plugin for MicClean (see above).
pub fn host_main(args: &[String]) {
    use std::io::Write;
    use windows_sys::Win32::System::Diagnostics::Debug::{SEM_FAILCRITICALERRORS, SEM_NOGPFAULTERRORBOX, SetErrorMode};
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::HiDpi::{DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext};
    // SAFETY: process-wide settings, before anything else runs. A crash just ends this process
    // (no "stopped working" box): MicClean notices. Plugin windows draw sharp at any scaling.
    unsafe {
        SetErrorMode(SEM_FAILCRITICALERRORS | SEM_NOGPFAULTERRORBOX);
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    MAIN_TID.store(tid(), Relaxed);
    let say = |m: &Msg| {
        let mut o = std::io::stdout().lock();
        let _ = serde_json::to_string(m).map(|l| writeln!(o, "{l}").and_then(|()| o.flush()));
    };
    let Some(name) = args.first() else { return };
    let mut first = String::new();
    let _ = std::io::stdin().read_line(&mut first);
    let Ok(Cmd::Load { info, state }) = serde_json::from_str(&first) else { return };
    let link = match Link::open(name) {
        Ok(l) => l,
        Err(e) => return say(&Msg::Failed(e)),
    };
    let mut plugin = match Loaded::load(&info, &from_hex(&state)) {
        Ok(p) => p,
        Err(e) => return say(&Msg::Failed(e)),
    };
    say(&Msg::Ready);

    let quit = Arc::new(AtomicBool::new(false));
    let audio = {
        let (local, quit) = (plugin.local.clone(), quit.clone());
        std::thread::spawn(move || {
            crate::audio::pro_audio_priority();
            serve(&link, &local, &quit);
        })
    };
    // Commands arrive on their own thread, which wakes this one for each.
    let main = unsafe { GetCurrentThreadId() };
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines().map_while(Result::ok) {
            if let Ok(c) = serde_json::from_str(&line)
                && tx.send(c).is_err()
            {
                return;
            }
            unsafe { wm::PostThreadMessageW(main, wm::WM_APP, 0, 0) };
        }
        // MicClean closed our input, or ended: stop.
        let _ = tx.send(Cmd::Quit);
        unsafe { wm::PostThreadMessageW(main, wm::WM_APP, 0, 0) };
    });
    let mut told = (false, None);
    'run: loop {
        // SAFETY: this thread's own message loop.
        unsafe {
            wm::MsgWaitForMultipleObjects(0, null(), 0, 15, wm::QS_ALLINPUT);
            let mut msg: wm::MSG = std::mem::zeroed();
            while wm::PeekMessageW(&mut msg, null_mut(), 0, 0, wm::PM_REMOVE) != 0 {
                wm::TranslateMessage(&msg);
                wm::DispatchMessageW(&msg);
            }
        }
        let mut asked = false;
        while let Ok(c) = rx.try_recv() {
            match c {
                Cmd::State => say(&Msg::State(to_hex(&plugin.state()))),
                Cmd::OpenEditor { dpi, dark } => {
                    plugin.open_editor(dpi, dark);
                    asked = true;
                }
                Cmd::CloseEditor => plugin.close_editor(),
                Cmd::Quit => break 'run,
                Cmd::Load { .. } => {}
            }
        }
        plugin.idle();
        let now = (plugin.editor_open(), plugin.editor_error.clone());
        if now != told || asked {
            say(&Msg::Editor { open: now.0, error: now.1.clone() });
            told = now;
        }
    }
    quit.store(true, Relaxed);
    let _ = audio.join();
    drop(plugin);
}

/// The plugin process's main thread (its plugin's GUI thread), for CLAP's thread check.
static MAIN_TID: AtomicU32 = AtomicU32::new(0);

fn tid() -> u32 {
    unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
}

fn on_main_thread() -> bool {
    tid() == MAIN_TID.load(Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
    use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, SetEvent, WaitForSingleObject};

    #[test]
    fn saved_state_survives_the_round_trip() {
        let b: Vec<u8> = (0..=255).collect();
        assert_eq!(from_hex(&to_hex(&b)), b);
        let s = join_state(b"comp", b"ctrl!");
        assert_eq!(split_state(&s), (&b"comp"[..], &b"ctrl!"[..]));
        assert_eq!(split_state(&[]), (&[][..], &[][..]));
    }

    /// The audio side of a plugin process, with a thread standing in for the process (it
    /// halves the sound): a frame comes back processed, one it sits on goes through dry
    /// without holding the audio up, and once it's done with that one they're back in step.
    #[test]
    fn sound_goes_through_shared_memory_and_a_late_plugin_leaves_it_dry() {
        let name = format!("MicClean-test-{}", std::process::id());
        let mine = Link::create(&name).unwrap();
        let theirs = Link::open(&name).unwrap();
        let (slow, quit) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let (s, q) = (slow.clone(), quit.clone());
        let fake = std::thread::spawn(move || {
            let shm = theirs.shm();
            while !q.load(Relaxed) {
                if unsafe { WaitForSingleObject(theirs.go, 50) } != WAIT_OBJECT_0 {
                    continue;
                }
                while s.load(Relaxed) {
                    std::thread::sleep(Duration::from_millis(1));
                }
                let n = shm.sent.load(Acquire);
                unsafe { (*shm.buf.get()).iter_mut().for_each(|x| *x *= 0.5) };
                shm.done.store(n, Release);
                unsafe { SetEvent(theirs.done) };
            }
        });
        let me = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, std::process::id()) };
        let health = Arc::new(Health::default());
        let mut p = Proc { link: mine, process: me, sent: 0, health: health.clone() };
        let mut x = [0.4f32; FRAME];
        p.process(&mut x);
        assert_eq!(x[0], 0.2, "processed");

        slow.store(true, Relaxed);
        let t = Instant::now();
        let mut x = [0.4f32; FRAME];
        p.process(&mut x);
        assert_eq!(x[0], 0.4, "late: dry");
        p.process(&mut x);
        assert_eq!(x[0], 0.4, "still on the late one: dry");
        // One wait of WAIT_MS (rounded up to Windows' timer), not two.
        assert!(t.elapsed() < Duration::from_millis(40), "waited {:?}", t.elapsed());
        assert_eq!(health.late.load(Relaxed), 2);

        slow.store(false, Relaxed);
        std::thread::sleep(Duration::from_millis(30));
        let mut x = [0.4f32; FRAME];
        p.process(&mut x);
        assert_eq!(x[0], 0.2, "back in step");
        assert_eq!(health.late.load(Relaxed), 0);
        assert!(!health.dead.load(Relaxed));
        quit.store(true, Relaxed);
        fake.join().unwrap();
    }

    /// The plugin MICCLEAN_PLUGIN="<path to the DLL>|<uid>|vst3 or clap" names.
    fn test_plugin() -> Info {
        let v = std::env::var("MICCLEAN_PLUGIN").expect("MICCLEAN_PLUGIN not set");
        let mut it = v.split('|');
        let (path, uid, f) = (it.next().unwrap(), it.next().unwrap(), it.next().unwrap());
        let format = if f == "clap" { Format::Clap } else { Format::Vst3 };
        let found = scan_file(format, Path::new(path)).unwrap();
        println!("found: {:?}", found.iter().map(|i| (&i.name, &i.uid)).collect::<Vec<_>>());
        found.iter().find(|i| i.uid == uid || uid.is_empty()).expect("uid not in the file").clone()
    }

    fn release_exe() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("target").join("release").join("micclean.exe")
    }

    /// The folder walk finds bundles and plain files, and a child copy of the release exe lists
    /// each: `cargo build --release` first, then `cargo test -- --ignored search`.
    #[test]
    #[ignore]
    fn search_finds_plugin_files_and_lists_them_in_a_child() {
        let all = files(&[]);
        let n = |f: Format| all.iter().filter(|(g, _)| *g == f).count();
        println!("{} VST3 files, {} CLAP files", n(Format::Vst3), n(Format::Clap));
        assert!(all.iter().all(|(_, p)| p.is_file()));
        for (f, p) in all.iter().filter(|(_, p)| p.to_string_lossy().contains("Nova") || p.to_string_lossy().contains("Pro-Q 4")) {
            let t = Instant::now();
            let found = scan_child(&release_exe(), *f, p);
            println!("{:?} {} -> {:?} in {:?}", f, p.display(), found.iter().map(|i| &i.name).collect::<Vec<_>>(), t.elapsed());
            assert!(!found.is_empty());
        }
    }

    /// Runs this thread's messages for `ms`, as a plugin process's loop would.
    fn pump(plugin: &mut Loaded, ms: u64) {
        let end = Instant::now() + Duration::from_millis(ms);
        while Instant::now() < end {
            unsafe {
                let mut msg: wm::MSG = std::mem::zeroed();
                while wm::PeekMessageW(&mut msg, null_mut(), 0, 0, wm::PM_REMOVE) != 0 {
                    wm::TranslateMessage(&msg);
                    wm::DispatchMessageW(&msg);
                }
            }
            plugin.idle();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Loads a real plugin right here (as its process does), runs sound through it and saves
    /// and restores its settings: `MICCLEAN_PLUGIN=... cargo test -- --ignored keeps_its`.
    /// MICCLEAN_PLUGIN_WINDOW=<ms> also opens its window that long.
    #[test]
    #[ignore]
    fn plugin_loads_processes_and_keeps_its_settings() {
        let info = test_plugin();
        MAIN_TID.store(tid(), Relaxed);
        let mut plugin = Loaded::load(&info, &[]).unwrap();
        let state = plugin.state();
        println!("{}: state {} bytes", info.name, state.len());
        let mut x = [0.0f32; FRAME];
        for _ in 0..150 {
            x.iter_mut().enumerate().for_each(|(i, s)| *s = (i as f32 * 0.05).sin() * 0.3);
            plugin.local.lock().unwrap().process(&mut x);
        }
        println!("energy out {:.2}", x.iter().map(|s| s * s).sum::<f32>());
        assert!(x.iter().all(|s| s.is_finite()));
        // Its window, behind your other windows (focus handed straight back), then closed.
        if let Some(ms) = std::env::var("MICCLEAN_PLUGIN_WINDOW").ok().and_then(|v| v.parse().ok()) {
            let fg = unsafe { wm::GetForegroundWindow() };
            plugin.open_editor(96, true);
            if let Some(e) = &plugin.editor {
                unsafe {
                    wm::SetForegroundWindow(fg);
                    wm::SetWindowPos(e.hwnd, wm::HWND_BOTTOM, 0, 0, 0, 0, wm::SWP_NOMOVE | wm::SWP_NOSIZE | wm::SWP_NOACTIVATE);
                }
            }
            println!("window: {:?}", plugin.editor.as_ref().map(|e| e.hwnd).ok_or(plugin.editor_error.clone()));
            pump(&mut plugin, ms);
            plugin.close_editor();
            pump(&mut plugin, 200);
        }
        drop(plugin);
        let again = Loaded::load(&info, &state).unwrap();
        assert!(!again.state().is_empty() || state.is_empty());
    }

    /// A real plugin in its own process, through Host as the app uses it: it starts, runs
    /// sound, hands its settings over, and when its process is killed MicClean hears of it and
    /// carries on dry without waiting. `cargo build --release`, then
    /// `MICCLEAN_PLUGIN=... cargo test -- --ignored own_process`.
    #[test]
    #[ignore]
    fn plugin_in_its_own_process_runs_and_a_crash_leaves_micclean_running() {
        let info = test_plugin();
        let mut host = Host { exe: Some(release_exe()), ..Host::default() };
        let mut entries = vec![Entry { key: 1, info, state: String::new(), crashed: false }];
        let t = Instant::now();
        let mut procs = None;
        while procs.is_none() && t.elapsed() < Duration::from_secs(30) {
            procs = host.sync(&[1], &mut entries).filter(|p| !p.is_empty());
            assert!(host.idle().is_empty(), "died while loading");
            std::thread::sleep(Duration::from_millis(20));
        }
        let procs = procs.expect("never got ready");
        println!("ready in {:?}", t.elapsed());
        let p = &procs[0].1;
        let mut x = [0.0f32; FRAME];
        let t = Instant::now();
        for _ in 0..300 {
            x.iter_mut().enumerate().for_each(|(i, s)| *s = (i as f32 * 0.05).sin() * 0.3);
            p.lock().unwrap().process(&mut x);
        }
        println!("300 frames in {:?}, energy out {:.2}, late {}", t.elapsed(), x.iter().map(|s| s * s).sum::<f32>(), p.lock().unwrap().health.late.load(Relaxed));
        assert!(x.iter().all(|s| s.is_finite()));
        host.store(&mut entries);
        println!("settings: {} bytes", entries[0].state.len() / 2);
        // Its window, from its own process, for MICCLEAN_PLUGIN_WINDOW ms.
        if let Some(ms) = std::env::var("MICCLEAN_PLUGIN_WINDOW").ok().and_then(|v| v.parse::<u64>().ok()) {
            let wait = |host: &mut Host, open: bool| {
                let t = Instant::now();
                while t.elapsed() < Duration::from_secs(5) {
                    assert!(host.idle().is_empty());
                    if let Some(Ok(i)) = host.get(1)
                        && (i.editor_open() == open || i.editor_error.is_some())
                    {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            };
            if let Some(Ok(i)) = host.get_mut(1) {
                i.open_editor(None, true);
            }
            wait(&mut host, true);
            let i = host.get(1).unwrap().as_ref().unwrap();
            println!("window open: {} {:?}", i.editor_open(), i.editor_error);
            assert!(i.editor_open());
            std::thread::sleep(Duration::from_millis(ms));
            host.get_mut(1).unwrap().as_mut().unwrap().close_editor();
            wait(&mut host, false);
            assert!(!host.get(1).unwrap().as_ref().unwrap().editor_open());
        }

        if let Some(Ok(inst)) = host.loaded.iter_mut().map(|(_, r)| r).next() {
            inst.child.kill().unwrap();
        }
        let t = Instant::now();
        let mut dead = Vec::new();
        while dead.is_empty() && t.elapsed() < Duration::from_secs(5) {
            p.lock().unwrap().process(&mut x);
            dead = host.idle();
            std::thread::sleep(Duration::from_millis(10));
        }
        println!("noticed in {:?}: {dead:?}", t.elapsed());
        assert_eq!(dead.len(), 1);
        let t = Instant::now();
        x.fill(0.25);
        p.lock().unwrap().process(&mut x);
        assert!(t.elapsed() < Duration::from_millis(2) && x[0] == 0.25, "dry, no wait");
        drop(procs);
        entries[0].crashed = true; // as MicClean marks it
        assert!(host.sync(&[1], &mut entries).is_some_and(|p| p.is_empty()));
        host.shutdown();
    }
}
