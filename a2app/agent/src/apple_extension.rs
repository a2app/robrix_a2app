//! Apple ExtensionFoundation host bridge, loaded at runtime.
//!
//! `libRobrixExtensionHost.dylib` (built by
//! `a2app/agent/extension/apple/build.sh`) implements the ExtensionFoundation
//! half in Swift — declaring the extension point, discovering an installed
//! `.appex`, and launching it over XPC. Rust reaches it through the small C
//! ABI below and wraps it in the same [`ExtensionBridge`]/[`ExtensionConnection`]
//! traits every other transport uses.
//!
//! Loading is `dlopen` rather than link-time on purpose: the dylib targets
//! macOS 26 (ExtensionFoundation's `AppExtensionPoint` era), so on older macOS
//! or an unbundled dev binary the load simply fails and Robrix keeps using the
//! confined child. Nothing here is reachable unless a bridge is installed.

use std::ffi::{c_char, c_void, CString};
use std::path::PathBuf;

use crate::extension::{ExtensionBridge, ExtensionConnection, ExtensionIdentity};

const RTLD_NOW: i32 = 2;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flag: i32) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

type AvailableFn = unsafe extern "C" fn() -> i32;
type LaunchFn = unsafe extern "C" fn() -> *mut c_void;
type SendFn = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;
type RecvFn = unsafe extern "C" fn(*mut c_void, *mut *mut u8, *mut usize) -> i32;
type FreeFn = unsafe extern "C" fn(*mut c_void);
type FreeBytesFn = unsafe extern "C" fn(*mut u8);

#[derive(Clone, Copy)]
struct Handles {
    available: AvailableFn,
    launch: LaunchFn,
    send: SendFn,
    recv: RecvFn,
    free: FreeFn,
    free_bytes: FreeBytesFn,
}

/// The Swift host bridge, wrapped for the Rust transport traits.
pub struct AppleExtensionBridge {
    handles: Handles,
}

// SAFETY: the bridge's C ABI is internally synchronized (the Swift side uses an
// NSCondition), and the handle it returns is stable for the process lifetime.
unsafe impl Send for AppleExtensionBridge {}
unsafe impl Sync for AppleExtensionBridge {}

impl AppleExtensionBridge {
    /// Locates `libRobrixExtensionHost.dylib` inside the running app bundle and
    /// loads it. Returns `None` when unbundled, on macOS < 26, or when the
    /// dylib is missing — all of which mean "no extension backend".
    pub fn load_from_bundle() -> Option<Self> {
        let Some(path) = bridge_path() else {
            makepad_widgets::log!("apple-bridge: not in an app bundle; extension unavailable");
            return None;
        };
        let c_path = CString::new(path.to_string_lossy().as_bytes()).ok()?;
        // SAFETY: `c_path` is a valid NUL-terminated C string.
        let library = unsafe { dlopen(c_path.as_ptr(), RTLD_NOW) };
        if library.is_null() {
            makepad_widgets::log!("apple-bridge: dlopen failed for {}", path.display());
            return None;
        }
        // SAFETY: each symbol is checked non-null before use; the pointers are
        // transmuted to the C signature the Swift `@_cdecl` functions declare.
        unsafe {
            let available: AvailableFn = match symbol(library, "robrix_ext_available") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_available missing");
                    return None;
                }
            };
            let launch: LaunchFn = match symbol(library, "robrix_ext_launch") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_launch missing");
                    return None;
                }
            };
            let send: SendFn = match symbol(library, "robrix_ext_send") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_send missing");
                    return None;
                }
            };
            let recv: RecvFn = match symbol(library, "robrix_ext_recv") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_recv missing");
                    return None;
                }
            };
            let free: FreeFn = match symbol(library, "robrix_ext_free") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_free missing");
                    return None;
                }
            };
            let free_bytes: FreeBytesFn = match symbol(library, "robrix_ext_free_bytes") {
                Some(f) => f,
                None => {
                    makepad_widgets::log!("apple-bridge: symbol robrix_ext_free_bytes missing");
                    return None;
                }
            };
            makepad_widgets::log!("apple-bridge: loaded {}", path.display());
            Some(Self {
                handles: Handles { available, launch, send, recv, free, free_bytes },
            })
        }
    }
}

/// Reads one exported symbol and reinterprets it as the requested fn type.
unsafe fn symbol<T: Copy>(library: *mut c_void, name: &str) -> Option<T> {
    let c_name = CString::new(name).ok()?;
    let pointer = unsafe { dlsym(library, c_name.as_ptr()) };
    if pointer.is_null() {
        None
    } else {
        Some(unsafe { std::mem::transmute_copy::<*mut c_void, T>(&pointer) })
    }
}

fn bridge_path() -> Option<PathBuf> {
    // <App>.app/Contents/MacOS/<exe> -> <App>.app/Contents/Frameworks/...
    let exe = std::env::current_exe().ok()?;
    let contents = exe.parent()?.parent()?;
    let path = contents.join("Frameworks").join("libRobrixExtensionHost.dylib");
    path.is_file().then_some(path)
}

impl ExtensionBridge for AppleExtensionBridge {
    fn discover(&self, _extension_point: &str) -> Vec<ExtensionIdentity> {
        // SAFETY: `available` was resolved from the loaded dylib.
        let found = unsafe { (self.handles.available)() } == 1;
        makepad_widgets::log!("apple-bridge: discover -> {found}");
        if found {
            vec![ExtensionIdentity {
                bundle_id: "robrix-agent-extension".to_string(),
                extension_point: crate::extension::AGENT_HOST_EXTENSION_POINT.to_string(),
                name: "Robrix Agent".to_string(),
            }]
        } else {
            Vec::new()
        }
    }

    fn launch(
        &self,
        _identity: &ExtensionIdentity,
    ) -> Result<Box<dyn ExtensionConnection>, String> {
        // SAFETY: `launch` was resolved from the loaded dylib.
        let handle = unsafe { (self.handles.launch)() };
        if handle.is_null() {
            makepad_widgets::log!("apple-bridge: launch returned null");
            Err("Could not launch the Robrix agent extension.".to_string())
        } else {
            makepad_widgets::log!("apple-bridge: launched connection");
            Ok(Box::new(AppleExtensionConnection { handle, handles: self.handles }))
        }
    }
}

/// One live XPC connection to the extension, as an [`ExtensionConnection`].
struct AppleExtensionConnection {
    handle: *mut c_void,
    handles: Handles,
}

// SAFETY: `send`/`recv`/`free` are thread-safe on the Swift side (NSCondition),
// and the handle is an opaque, stable pointer for the connection's lifetime.
unsafe impl Send for AppleExtensionConnection {}
unsafe impl Sync for AppleExtensionConnection {}

impl ExtensionConnection for AppleExtensionConnection {
    fn send(&self, message: &[u8]) -> Result<(), String> {
        // SAFETY: `message` is a live slice for the call; `handle` is valid.
        let ok = unsafe { (self.handles.send)(self.handle, message.as_ptr(), message.len()) };
        makepad_widgets::log!("apple-bridge: send {} bytes -> ok={}", message.len(), ok);
        if ok == 1 {
            Ok(())
        } else {
            Err("The agent extension refused the frame.".to_string())
        }
    }

    fn recv(&self) -> Option<Vec<u8>> {
        let mut pointer: *mut u8 = std::ptr::null_mut();
        let mut length: usize = 0;
        // SAFETY: the out-pointers point at valid stack storage for the call.
        let ok = unsafe { (self.handles.recv)(self.handle, &mut pointer, &mut length) };
        if ok != 1 || pointer.is_null() {
            makepad_widgets::log!("apple-bridge: recv closed (ok={ok}, ptr_null={})", pointer.is_null());
            return None;
        }
        // SAFETY: on success the Swift side allocated `length` bytes and hands
        // ownership to us until `free_bytes` is called.
        let bytes = unsafe { std::slice::from_raw_parts(pointer, length) }.to_vec();
        unsafe { (self.handles.free_bytes)(pointer) };
        makepad_widgets::log!("apple-bridge: recv {} bytes: {}", bytes.len(), String::from_utf8_lossy(&bytes[..bytes.len().min(200)]));
        Some(bytes)
    }

    fn close(&self) {
        makepad_widgets::log!("apple-bridge: close connection");
        // SAFETY: `free` was resolved from the dylib and is called once.
        unsafe { (self.handles.free)(self.handle) };
    }
}
