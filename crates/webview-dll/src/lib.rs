use once_cell::sync::Lazy;
use std::mem::transmute;
use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32};
use std::sync::mpsc::{Sender, channel};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::Input::*;
use windows::Win32::{
    Foundation::*,
    System::{Diagnostics::Debug::*, SystemServices::*, Threading::*},
    UI::{Input::KeyboardAndMouse::*, WindowsAndMessaging::*},
};
use windows::core::*;

static SPACE_DOWN: INPUT = INPUT {
    r#type: INPUT_KEYBOARD,
    Anonymous: INPUT_0 {
        ki: KEYBDINPUT {
            wVk: VK_SPACE,
            wScan: 0,
            dwFlags: KEYBD_EVENT_FLAGS(0),
            time: 0,
            dwExtraInfo: 0,
        },
    },
};

static SPACE_UP: INPUT = INPUT {
    r#type: INPUT_KEYBOARD,
    Anonymous: INPUT_0 {
        ki: KEYBDINPUT {
            wVk: VK_SPACE,
            wScan: 0,
            dwFlags: KEYEVENTF_KEYUP,
            time: 0,
            dwExtraInfo: 0,
        },
    },
};

static SCROLL_SENDER: Lazy<Sender<()>> = Lazy::new(|| {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        while rx.recv().is_ok() {
            unsafe {
                SendInput(&[SPACE_DOWN], std::mem::size_of::<INPUT>() as i32);
                Sleep(5);
                SendInput(&[SPACE_UP], std::mem::size_of::<INPUT>() as i32);
            }
        }
    });
    tx
});

static mut PREV_WNDPROC_1: WNDPROC = None;
static mut PREV_WNDPROC_2: WNDPROC = None;

static LOCK_STATUS: AtomicBool = AtomicBool::new(false);
static WINDOW_HANDLE: AtomicPtr<HWND> = AtomicPtr::new(std::ptr::null_mut());
static HOOK_HANDLE: AtomicPtr<HWINEVENTHOOK> = AtomicPtr::new(std::ptr::null_mut());

struct ChromeWindows {
    chrome_window: HWND,
    chrome_renderwidget: HWND,
}

impl ChromeWindows {
    fn get(parent: HWND) -> Self {
        ChromeWindows {
            chrome_window: Self::find_child_window_by_class(parent, "Chrome_WidgetWin_1"),
            chrome_renderwidget: Self::find_child_window_by_class(
                parent,
                "Chrome_RenderWidgetHostHWND",
            ),
        }
    }

    #[allow(clippy::fn_to_numeric_cast)]
    unsafe fn set_window_procs(&self) {
        unsafe {
            // set proc for chrome_window
            let original_proc_1 = GetWindowLongPtrW(self.chrome_window, GWLP_WNDPROC);
            PREV_WNDPROC_1 = transmute::<
                isize,
                Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
            >(original_proc_1);
            SetWindowLongPtrW(self.chrome_window, GWLP_WNDPROC, wnd_proc_1 as isize);

            // set proc for chrome_renderwidget
            let original_proc_2 = GetWindowLongPtrW(self.chrome_renderwidget, GWLP_WNDPROC);
            PREV_WNDPROC_2 = transmute::<
                isize,
                Option<unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT>,
            >(original_proc_2);
            SetWindowLongPtrW(
                self.chrome_renderwidget,
                GWLP_WNDPROC,
                wnd_proc_widget as isize,
            );
        }
    }

    fn find_child_window_by_class(parent: HWND, class_name: &str) -> HWND {
        unsafe {
            let mut data = (HWND::default(), class_name);

            if let BOOL(1) = EnumChildWindows(
                Some(parent),
                Some(find_child_window),
                LPARAM(&mut data as *mut (HWND, &str) as _),
            ) {
                OutputDebugStringW(w!("Enum Child Windows Failed\0"));
            }

            data.0
        }
    }
}

#[unsafe(no_mangle)]
extern "system" fn DllMain(_: HINSTANCE, call_reason: u32, _: *mut ()) {
    match call_reason {
        DLL_PROCESS_ATTACH => attach(),
        DLL_PROCESS_DETACH => detach(),
        _ => (),
    }
}

static THREAD_ID: AtomicU32 = AtomicU32::new(0);

fn detach() {
    unsafe {
        let hook_handle = HOOK_HANDLE.load(std::sync::atomic::Ordering::Relaxed);
        if !hook_handle.is_null() {
            let _ = UnhookWinEvent(*hook_handle);
            drop(Box::from_raw(hook_handle));
        }

        //  terminate the message loop otherwise launching just crashes if webview2 is still running
        let thread_id = THREAD_ID.load(std::sync::atomic::Ordering::Relaxed);
        if thread_id != 0 {
            PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)).ok();
        }
    }
}

fn attach() {
    unsafe {
        let parent = FindWindowW(w!("krunker_webview"), PCWSTR::null()).unwrap();
        let handle_ptr = Box::into_raw(Box::new(parent)); // store on the heap so it stays alive
        WINDOW_HANDLE.store(handle_ptr, std::sync::atomic::Ordering::Relaxed);
        let chrome_windows = ChromeWindows::get(parent);
        chrome_windows.set_window_procs();
        
        // Register raw input for low-latency mouse
        register_raw_mouse(parent, true);

        std::thread::spawn(move || {
            THREAD_ID.store(GetCurrentThreadId(), std::sync::atomic::Ordering::Relaxed);
            
            // Enable MMCSS for input thread
            enable_input_thread_mmcss();
            
            let mut msg: MSG = MSG::default();
            // check whenever a window is created if it has the attribute Chrome.WindowTranslucent (the one that warns about pointer lock) and if it does, destroy it
            let hook = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_CREATE,
                None,
                Some(window_event_proc),
                GetCurrentProcessId(),
                0,
                WINEVENT_OUTOFCONTEXT,
            );
            let hook_ptr = Box::into_raw(Box::new(hook));
            HOOK_HANDLE.store(hook_ptr, std::sync::atomic::Ordering::Relaxed);

            loop {
                if GetMessageW(&mut msg, None, 0, 0).into() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                } else {
                    OutputDebugStringW(w!("killing myself\0"));
                    break;
                }
            }
        });
    }
}

extern "system" fn find_child_window(handle: HWND, lparam: LPARAM) -> BOOL {
    unsafe {
        let data = lparam.0 as *mut (HWND, &str);
        let target_class = (*data).1;

        let mut class_name: [u16; 256] = [0; 256];
        GetClassNameW(handle, &mut class_name);

        let window_class = String::from_utf16_lossy(&class_name);

        if window_class.contains(target_class) {
            (*data).0 = handle;
            return BOOL(0);
        }

        BOOL(1)
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn wnd_proc_1(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_CHAR => LRESULT(1),
            WM_QUIT => {
                detach();
                CallWindowProcW(PREV_WNDPROC_1, window, message, wparam, lparam)
            }
            // when you press esc chromium puts a few seconds of delay before the pointer can get locked again as a security measure
            WM_KEYDOWN | WM_KEYUP => {
                if wparam.0 == VK_ESCAPE.0 as usize
                    && LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed)
                {
                    // glorp.exe (not the webview)
                    let glorp = WINDOW_HANDLE.load(std::sync::atomic::Ordering::Relaxed);
                    SetFocus(Some(*glorp)).ok();
                }
                CallWindowProcW(PREV_WNDPROC_1, window, message, wparam, lparam)
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK | WM_RBUTTONDOWN | WM_RBUTTONDBLCLK => {
                CallWindowProcW(
                    PREV_WNDPROC_1,
                    window,
                    message,
                    WPARAM(wparam.0 & !MK_LBUTTON.0 as usize),
                    lparam,
                )
            }
            WM_MOUSEMOVE => {
                if LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed) {
                    return CallWindowProcW(
                        PREV_WNDPROC_1,
                        window,
                        message,
                        WPARAM(wparam.0 & !MK_LBUTTON.0 as usize),
                        lparam,
                    );
                }
                CallWindowProcW(PREV_WNDPROC_1, window, message, wparam, lparam)
            }
            WM_INPUT => {
                // Use efficient batched reading for all queued input (telemetry/monitoring)
                drain_raw_input_buffer(|rip| {
                    if rip.header.dwType == RIM_TYPEMOUSE.0 {
                        handle_raw_mouse(&rip.data.mouse);
                    }
                });
                
                // Always forward to Chromium since we're not using RIDEV_NOLEGACY
                // Both raw input (for telemetry) and legacy WM_MOUSE* (for Chromium) coexist
                return CallWindowProcW(PREV_WNDPROC_1, window, message, wparam, lparam);
            }
            0x00FE => { // WM_INPUT_DEVICE_CHANGE
                // wparam: GIDC_ARRIVAL (1) or GIDC_REMOVAL (2)
                // lparam: HANDLE to the device
                // Could refresh cached device info with GetRawInputDeviceInfo here
                return LRESULT(0);
            }
            _ => CallWindowProcW(PREV_WNDPROC_1, window, message, wparam, lparam),
        }
    }
}

#[allow(clippy::fn_to_numeric_cast)]
#[unsafe(no_mangle)]
unsafe extern "system" fn wnd_proc_widget(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_APP => {
                SetWindowLongPtrW(window, GWLP_WNDPROC, wnd_proc_widget_rampboost as isize);
                LRESULT(1)
            }
            WM_USER => {
                let locked = wparam.0 != 0;
                LOCK_STATUS.store(locked, std::sync::atomic::Ordering::Relaxed);
                
                // NOTE: Not switching to NOLEGACY mode yet - need to implement delta forwarding first
                // Currently keeping legacy WM_MOUSE* messages enabled so Chromium continues to work
                // TODO: Re-enable dynamic mode switching once raw delta forwarding to JS is implemented
                
                LRESULT(1)
            }
            WM_MOUSEWHEEL => {
                if LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed) {
                    let glorp = WINDOW_HANDLE.load(std::sync::atomic::Ordering::Relaxed);
                    // Forward to main window for JS event processing
                    PostMessageW(Some(*glorp), message, wparam, lparam).ok();
                    return LRESULT(1);
                }
                CallWindowProcW(PREV_WNDPROC_2, window, message, wparam, lparam)
            }
            _ => CallWindowProcW(PREV_WNDPROC_2, window, message, wparam, lparam),
        }
    }
}

#[unsafe(no_mangle)]
unsafe extern "system" fn wnd_proc_widget_rampboost(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe {
        match message {
            WM_MOUSEWHEEL => {
                if LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed) {
                    SCROLL_SENDER.send(()).ok();
                    return LRESULT(1);
                }
                CallWindowProcW(PREV_WNDPROC_2, window, message, wparam, lparam)
            }
            WM_USER => {
                LOCK_STATUS.store(wparam.0 != 0, std::sync::atomic::Ordering::Relaxed);
                LRESULT(1)
            }
            _ => CallWindowProcW(PREV_WNDPROC_2, window, message, wparam, lparam),
        }
    }
}

unsafe extern "system" fn window_event_proc(
    _hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    unsafe {
        let prop = GetPropW(hwnd, w!("Chrome.WindowTranslucent"));
        if !prop.is_invalid() {
            PostMessageW(Some(hwnd), WM_DESTROY, WPARAM(0), LPARAM(0)).ok();
        }
    }
}

// ---------- Raw Input Setup ----------

/// Call this once at startup with unlocked mode
unsafe fn register_raw_mouse(hwnd: HWND, background: bool) {
    unsafe { register_raw_mouse_mode(hwnd, background, false) };
}

/// Register raw mouse input with dynamic mode switching
/// nolegacy=true: suppress WM_MOUSE* (pointer lock, low latency)
/// nolegacy=false: allow WM_MOUSE* (normal Chromium input)
unsafe fn register_raw_mouse_mode(hwnd: HWND, background: bool, nolegacy: bool) {
    use windows::Win32::UI::Input::*;
    
    let mut flags = RIDEV_DEVNOTIFY; // Always get device change notifications
    
    if background {
        flags |= RIDEV_INPUTSINK; // Receive input even when not in foreground
    }
    
    if nolegacy {
        // POINTER LOCK MODE: Suppress legacy WM_MOUSE* for lowest latency
        flags |= RIDEV_NOLEGACY; // No WM_MOUSEMOVE, WM_LBUTTONDOWN, etc.
        unsafe { OutputDebugStringW(w!("Raw input: NOLEGACY mode (pointer locked)\0")); }
    } else {
        // NORMAL MODE: Allow legacy messages for Chromium
        unsafe { OutputDebugStringW(w!("Raw input: Legacy mode (Chromium input)\0")); }
    }
    
    let rid = RAWINPUTDEVICE {
        usUsagePage: 0x01, // HID_USAGE_PAGE_GENERIC
        usUsage: 0x02,     // HID_USAGE_GENERIC_MOUSE
        dwFlags: flags,
        hwndTarget: hwnd,
    };
    
    unsafe {
        match RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32) {
            Ok(_) => {
                // Success - mode switched
            }
            Err(e) => {
                let msg = format!("Failed to register raw input mode: {:?}\0", e);
                let wide: Vec<u16> = msg.encode_utf16().collect();
                OutputDebugStringW(PCWSTR(wide.as_ptr()));
            }
        }
    }
}

/// Read raw input data with correct buffer sizing (single message)
#[allow(dead_code)]
unsafe fn read_raw_input(lparam: LPARAM) -> Option<RAWINPUT> {
    use windows::Win32::UI::Input::*;
    
    let hraw = HRAWINPUT(lparam.0 as _);
    let mut size: u32 = 0;
    
    // First call: query size
    unsafe {
        GetRawInputData(
            hraw,
            RID_INPUT,
            None,
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
    }
    
    if size == 0 {
        return None;
    }
    
    // Allocate buffer with correct size
    let mut buf = vec![0u8; size as usize];
    let got = unsafe {
        GetRawInputData(
            hraw,
            RID_INPUT,
            Some(buf.as_mut_ptr() as _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        )
    };
    
    if got == u32::MAX || got == 0 {
        return None;
    }
    
    // SAFETY: buffer contains a valid RAWINPUT structure
    unsafe { Some(*(buf.as_ptr() as *const RAWINPUT)) }
}

// Reusable thread-local buffer to avoid allocations on each message
thread_local! {
    static RAWBUF: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Drain all queued raw input using GetRawInputBuffer (more efficient than one-by-one)
unsafe fn drain_raw_input_buffer(mut handle_one: impl FnMut(&RAWINPUT)) {
    use windows::Win32::UI::Input::*;
    
    // Step 1: Query required size (in bytes) for all queued RAWINPUTs
    let mut bytes_needed: u32 = 0;
    unsafe {
        GetRawInputBuffer(
            None,
            &mut bytes_needed,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
    }
    
    if bytes_needed == 0 {
        return;
    }
    
    RAWBUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        if buf.len() < bytes_needed as usize {
            buf.resize(bytes_needed as usize, 0);
        }
        
        // Step 2: Fetch all RAWINPUTs
        let mut struct_count = (buf.len() / std::mem::size_of::<RAWINPUT>()) as u32;
        let got = unsafe {
            GetRawInputBuffer(
                Some(buf.as_mut_ptr() as *mut RAWINPUT),
                &mut struct_count,
                std::mem::size_of::<RAWINPUTHEADER>() as u32,
            )
        };
        
        if got == u32::MAX || got == 0 {
            return;
        }
        
        // Step 3: Iterate through each RAWINPUT structure
        let mut offset = 0usize;
        for _ in 0..got {
            unsafe {
                let rip: &RAWINPUT = &*(buf.as_ptr().add(offset) as *const RAWINPUT);
                handle_one(rip);
                // Advance to next RAWINPUT using dwSize
                offset += rip.header.dwSize as usize;
            }
        }
    });
}

/// Drain all WM_INPUT messages from the queue (call before present)
#[allow(dead_code)]
unsafe fn pump_all_rawinput(hwnd: HWND) {
    use windows::Win32::UI::Input::*;
    
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // Drain only WM_INPUT messages to minimize latency
    while unsafe { PeekMessageW(&mut msg, Some(hwnd), WM_INPUT, WM_INPUT, PM_REMOVE) }.into() {
        // Process using buffered read
        unsafe {
            drain_raw_input_buffer(|rip| {
                if rip.header.dwType == RIM_TYPEMOUSE.0 {
                    handle_raw_mouse(&rip.data.mouse);
                }
            });
        }
    }
}

/// Handle a single raw mouse input sample
unsafe fn handle_raw_mouse(mouse: &RAWMOUSE) {
    let button_flags = unsafe { mouse.Anonymous.Anonymous.usButtonFlags };
    
    // Handle wheel input first (even when not locked, for consistency)
    if (button_flags & RI_MOUSE_WHEEL as u16) != 0 {
        let wheel_delta = unsafe { (mouse.Anonymous.Anonymous.usButtonData as i16) as i32 };
        // Forward vertical wheel delta (multiples of WHEEL_DELTA = 120)
        // This is where you'd send the wheel event to JS/game layer
        
        if LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed) {
            // During pointer lock, use the SCROLL_SENDER for ramp boost
            if wheel_delta != 0 {
                SCROLL_SENDER.send(()).ok();
            }
        }
    }
    
    if (button_flags & RI_MOUSE_HWHEEL as u16) != 0 {
        let _hwheel_delta = unsafe { (mouse.Anonymous.Anonymous.usButtonData as i16) as i32 };
        // Forward horizontal wheel delta if needed
    }
    
    // Check if this is relative motion (typical for mice)
    let is_relative = (mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0) == 0;
    if is_relative && LOCK_STATUS.load(std::sync::atomic::Ordering::Relaxed) {
        let dx = mouse.lLastX;
        let dy = mouse.lLastY;
        
        // Ignore if no actual motion
        if dx != 0 || dy != 0 {
            // Forward dx, dy to game/JS layer here
            // This is where you'd inject into your input pipeline
            // Raw input provides these deltas without coalescing
        }
    }
}

/// Enable MMCSS scheduling for the input thread
unsafe fn enable_input_thread_mmcss() {
    #[link(name = "Avrt")]
    unsafe extern "system" {
        fn AvSetMmThreadCharacteristicsW(task_name: PCWSTR, task_index: *mut u32) -> HANDLE;
        fn AvSetMmThreadPriority(avrt_handle: HANDLE, priority: i32) -> BOOL;
    }
    
    let task_name: Vec<u16> = "Games".encode_utf16().chain(Some(0)).collect();
    let mut task_index = 0u32;
    let handle = unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(task_name.as_ptr()), &mut task_index) };
    
    if !handle.is_invalid() {
        unsafe {
            let _ = AvSetMmThreadPriority(handle, 1); // AVRT_PRIORITY_HIGH
            SetThreadPriorityBoost(GetCurrentThread(), true).ok(); // Disable dynamic boost for consistency
            OutputDebugStringW(w!("Input thread MMCSS enabled\0"));
        }
    }
}
