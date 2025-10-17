#![allow(non_snake_case)]
use std::{collections::HashMap, ffi::c_void, mem::ManuallyDrop, sync::Mutex};

use once_cell::sync::Lazy;
use minhook::MinHook;

use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::*;

use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Direct3D::*;
use windows::Win32::Graphics::Direct3D11::*;

// MMCSS FFI bindings
#[link(name = "Avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: PCWSTR, task_index: *mut u32) -> HANDLE;
    fn AvSetMmThreadPriority(avrt_handle: HANDLE, priority: i32) -> BOOL;
}

const AVRT_PRIORITY_HIGH: i32 = 1;

// ---------- globals ----------
static WAIT_HANDLES: Lazy<Mutex<HashMap<usize, usize>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static mut TEARING_SUPPORTED: bool = false;

static mut ORIGINAL_CREATE_SC_COMP: Option<
    unsafe extern "system" fn(
        this: *mut c_void,
        pdevice: *mut c_void,
        pdesc: *const DXGI_SWAP_CHAIN_DESC1,
        prestricttooutput: *mut c_void,
        ppswapchain: *mut *mut c_void,
    ) -> HRESULT,
> = None;

static mut ORIGINAL_CREATE_SC_SURFACE: Option<
    unsafe extern "system" fn(
        this: *mut c_void,
        pdevice: *mut c_void,
        pdesc: *const DXGI_SWAP_CHAIN_DESC1,
        surface: HANDLE,
        ppswapchain: *mut *mut c_void,
    ) -> HRESULT,
> = None;

static mut ORIGINAL_PRESENT1: Option<
    unsafe extern "system" fn(
        this: *mut c_void,
        sync_interval: u32,
        present_flags: DXGI_PRESENT,
        p_params: *const DXGI_PRESENT_PARAMETERS,
    ) -> HRESULT,
> = None;

// ---------- tiny MMCSS helper (register once per thread) ----------
thread_local! {
    static MMCSS_COOKIE: std::cell::Cell<Option<HANDLE>> = std::cell::Cell::new(None);
}
fn ensure_mmcss_games_high() {
    MMCSS_COOKIE.with(|tls| {
        if tls.get().is_none() {
            let mut task_index = 0u32;
            let wide_name: Vec<u16> = "Games".encode_utf16().chain(Some(0)).collect();
            let cookie = unsafe {
                AvSetMmThreadCharacteristicsW(PCWSTR(wide_name.as_ptr()), &mut task_index)
            };
            if !cookie.is_invalid() {
                unsafe {
                    let _ = AvSetMmThreadPriority(cookie, AVRT_PRIORITY_HIGH);
                    // Disable dynamic priority boost for consistent scheduling
                    SetThreadPriorityBoost(GetCurrentThread(), true).ok();
                }
                tls.set(Some(cookie));
            }
        }
    });
}

// ---------- helpers ----------
fn patch_desc(desc: &mut DXGI_SWAP_CHAIN_DESC1) {
    desc.SwapEffect = DXGI_SWAP_EFFECT_FLIP_DISCARD;
    desc.BufferCount = 2;
    desc.AlphaMode = DXGI_ALPHA_MODE_IGNORE; // opaque helps independent flip
    desc.Scaling = DXGI_SCALING_NONE; // no scaling helps independent flip
    desc.Flags |= DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32;
    unsafe {
        if TEARING_SUPPORTED {
            desc.Flags |= DXGI_SWAP_CHAIN_FLAG_ALLOW_TEARING.0 as u32;
        }
    }
}

unsafe fn after_create_store_wait(this_sc: *mut *mut c_void) {
    // Don't auto-Release app's COM object
    let sc1 = ManuallyDrop::new(unsafe { IDXGISwapChain1::from_raw(*this_sc) });
    if let Ok(sc2) = (&*sc1).cast::<IDXGISwapChain2>() {
        // Set maximum frame latency to 1 for lowest latency
        let _ = unsafe { sc2.SetMaximumFrameLatency(1) };
        // Get the waitable object handle
        let h = unsafe { sc2.GetFrameLatencyWaitableObject() };
        // h is a HANDLE; store as usize to avoid Send/Sync problems
        WAIT_HANDLES.lock().unwrap().insert(unsafe { *this_sc as usize }, h.0 as usize);
        std::mem::forget(sc2);
    }
}

// ---------- hooks ----------
unsafe extern "system" fn create_sc_for_composition_hk(
    this: *mut c_void,
    pdevice: *mut c_void,
    pdesc: *const DXGI_SWAP_CHAIN_DESC1,
    prestricttooutput: *mut c_void,
    ppswapchain: *mut *mut c_void,
) -> HRESULT {
    let Some(orig) = (unsafe { ORIGINAL_CREATE_SC_COMP }) else { return E_FAIL.into(); };
    let mut desc = unsafe { *pdesc };
    patch_desc(&mut desc);
    let hr = unsafe { orig(this, pdevice, &desc, prestricttooutput, ppswapchain) };
    if hr.is_ok() { unsafe { after_create_store_wait(ppswapchain) }; }
    hr
}

unsafe extern "system" fn create_sc_for_surface_handle_hk(
    this: *mut c_void,
    pdevice: *mut c_void,
    pdesc: *const DXGI_SWAP_CHAIN_DESC1,
    surface: HANDLE,
    ppswapchain: *mut *mut c_void,
) -> HRESULT {
    let Some(orig) = (unsafe { ORIGINAL_CREATE_SC_SURFACE }) else { return E_FAIL.into(); };
    let mut desc = unsafe { *pdesc };
    patch_desc(&mut desc);
    let hr = unsafe { orig(this, pdevice, &desc, surface, ppswapchain) };
    if hr.is_ok() { unsafe { after_create_store_wait(ppswapchain) }; }
    hr
}

unsafe extern "system" fn present_hk(
    this: *mut c_void,
    sync_interval: u32,
    mut present_flags: DXGI_PRESENT,
    p_params: *const DXGI_PRESENT_PARAMETERS,
) -> HRESULT {
    // MMCSS on the actual presenter thread
    ensure_mmcss_games_high();

    // CRITICAL: Wait on the waitable object BEFORE presenting
    // This ensures queue depth = 1, minimizing latency
    if let Some(&h_usize) = WAIT_HANDLES.lock().unwrap().get(&(this as usize)) {
        let h = HANDLE(h_usize as *mut c_void);
        // Wait until the previously-presented frame is displayed
        let _ = unsafe { WaitForSingleObjectEx(h, INFINITE, true) };
    }

    // Enable tearing for VRR/uncapped when sync_interval == 0
    if unsafe { TEARING_SUPPORTED } && sync_interval == 0 {
        present_flags |= DXGI_PRESENT_ALLOW_TEARING;
    }

    let Some(orig) = (unsafe { ORIGINAL_PRESENT1 }) else { return E_FAIL.into(); };
    unsafe { orig(this, sync_interval, present_flags, p_params) }
}

// ---------- install ----------
unsafe fn install_hooks() {
    // Make a small factory to resolve vtables and tearing support once
    let Ok(factory) = (unsafe { CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) }) else { return; };
    
    if let Ok(f5) = factory.cast::<IDXGIFactory5>() {
        let mut ok = BOOL(0);
        let _ = unsafe {
            f5.CheckFeatureSupport(
                DXGI_FEATURE_PRESENT_ALLOW_TEARING,
                &mut ok as *mut _ as _,
                std::mem::size_of::<BOOL>() as u32,
            )
        };
        unsafe { TEARING_SUPPORTED = ok.as_bool(); }
    }

    // Hook IDXGIFactory2::CreateSwapChainForComposition
    let create_comp_ptr =
        unsafe { (*(factory.as_raw() as *const *const c_void)).offset(15) as *mut c_void };
    let tramp1 = unsafe { MinHook::create_hook(create_comp_ptr, create_sc_for_composition_hk as *mut c_void).unwrap() };
    unsafe { ORIGINAL_CREATE_SC_COMP = std::mem::transmute(tramp1) };

    // Hook IDXGIFactoryMedia::CreateSwapChainForCompositionSurfaceHandle
    if let Ok(fm) = factory.cast::<IDXGIFactoryMedia>() {
        let create_surface_ptr =
            unsafe { (*(fm.as_raw() as *const *const c_void)).offset(3) as *mut c_void };
        let tramp2 = unsafe { MinHook::create_hook(create_surface_ptr, create_sc_for_surface_handle_hk as *mut c_void).unwrap() };
        unsafe { ORIGINAL_CREATE_SC_SURFACE = std::mem::transmute(tramp2) };
    }

    // Create a tiny dummy swapchain to grab Present1 vtbl and hook it
    let (device, sc) = unsafe { make_dummy_swapchain(&factory) };
    let present1_ptr = sc.vtable().Present1 as *mut c_void;
    let tramp3 = unsafe { MinHook::create_hook(present1_ptr, present_hk as *mut c_void).unwrap() };
    unsafe { ORIGINAL_PRESENT1 = std::mem::transmute(tramp3) };
    std::mem::forget(device);
    std::mem::forget(sc);

    unsafe { MinHook::enable_all_hooks().unwrap() };
}

unsafe fn make_dummy_swapchain(factory2: &IDXGIFactory2) -> (ID3D11Device, IDXGISwapChain1) {
    let mut dev: Option<ID3D11Device> = None;
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_SINGLETHREADED,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut dev),
            None,
            None,
        ).ok().unwrap();
    }
    let dev = dev.unwrap();

    // 1x1 composition swapchain
    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 1,
        Height: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: BOOL(0),
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    let sc = unsafe { factory2.CreateSwapChainForComposition(&dev, &desc, None).unwrap() };
    (dev, sc)
}

// ---------- DllMain ----------
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DllMain(_hinst: HINSTANCE, reason: u32, _reserved: *mut ()) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        std::thread::spawn(|| unsafe { install_hooks() });
    }
    TRUE
}
