// modules/mmcss.rs
use windows::{
    core::*,
    Win32::{
        Foundation::*,
        System::Threading::*,
    },
};

#[link(name = "Avrt")]
unsafe extern "system" {
    fn AvSetMmThreadCharacteristicsW(task_name: PCWSTR, task_index: *mut u32) -> HANDLE;
    fn AvSetMmThreadPriority(avrt_handle: HANDLE, priority: AvrtPriority) -> BOOL;
    fn AvRevertMmThreadCharacteristics(avrt_handle: HANDLE) -> BOOL;
}

#[repr(i32)]
#[allow(dead_code)]
enum AvrtPriority {
    Low = -1,
    Normal = 0,
    High = 1,
    Critical = 2,
}

pub struct MmcssHandle {
    handle: HANDLE,
}

impl MmcssHandle {
    /// Register the current thread with MMCSS
    /// task_name: "Games", "Pro Audio", "Window Manager", etc.
    pub fn register(task_name: &str) -> Result<Self> {
        unsafe {
            let wide_name: Vec<u16> = task_name.encode_utf16().chain(Some(0)).collect();
            let mut task_index: u32 = 0;
            
            let handle = AvSetMmThreadCharacteristicsW(
                PCWSTR(wide_name.as_ptr()),
                &mut task_index,
            );
            
            if handle.is_invalid() {
                return Err(Error::from_win32());
            }
            
            Ok(Self { handle })
        }
    }
    
    /// Set thread priority within MMCSS
    pub fn set_priority(&self, priority: MmcssPriority) -> Result<()> {
        unsafe {
            let avrt_priority = match priority {
                MmcssPriority::Low => AvrtPriority::Low,
                MmcssPriority::Normal => AvrtPriority::Normal,
                MmcssPriority::High => AvrtPriority::High,
                MmcssPriority::Critical => AvrtPriority::Critical,
            };
            
            let result = AvSetMmThreadPriority(self.handle, avrt_priority);
            
            if result.as_bool() {
                Ok(())
            } else {
                Err(Error::from_win32())
            }
        }
    }
}

impl Drop for MmcssHandle {
    fn drop(&mut self) {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(self.handle);
        }
    }
}

#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub enum MmcssPriority {
    Low,
    Normal,
    High,
    Critical,
}

/// Register the webview process with MMCSS
pub fn register_webview_process(webview_pid: u32, task_class: &str) -> Result<()> {
    unsafe {
        let process_handle = OpenProcess(
            PROCESS_SET_INFORMATION,
            false,
            webview_pid,
        )?;
        
        // Set process priority class to HIGH_PRIORITY_CLASS for better scheduling
        if SetPriorityClass(process_handle, HIGH_PRIORITY_CLASS).is_ok() {
            println!("Set webview process to HIGH_PRIORITY_CLASS");
        }
        
        CloseHandle(process_handle).ok();
    }
    
    // Register the current thread with MMCSS
    let task_class_owned = task_class.to_string();
    std::thread::spawn(move || {
        if let Ok(mmcss) = MmcssHandle::register(&task_class_owned) {
            if mmcss.set_priority(MmcssPriority::High).is_ok() {
                println!("MMCSS registered for task class: {}", task_class_owned);
            }
            
            // Keep this thread alive to maintain MMCSS registration
            loop {
                std::thread::park();
            }
        } else {
            eprintln!("Failed to register MMCSS");
        }
    });
    
    Ok(())
}

/// Apply MMCSS to the current thread
#[allow(dead_code)]
pub fn apply_to_current_thread(task_class: &str, priority: MmcssPriority) -> Result<MmcssHandle> {
    let handle = MmcssHandle::register(task_class)?;
    handle.set_priority(priority)?;
    Ok(handle)
}