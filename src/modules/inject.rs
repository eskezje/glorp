use windows::core::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::LibraryLoader::*;
use windows::Win32::System::Memory::{VirtualAllocEx, MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE};
use windows::Win32::System::Threading::*;

pub fn inject_into_child_gpu_process(browser_pid: u32, dll_path: &str) -> Result<()> {
    unsafe {
        // Find child msedgewebview2.exe that is NOT the browser pid
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)?;
        let mut pe = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut pe).is_err() {
            CloseHandle(snap).ok();
            return Err(Error::from(E_FAIL));
        }
        let mut target_pid: Option<u32> = None;

        loop {
            let name = String::from_utf16_lossy(&pe.szExeFile).trim_matches('\0').to_lowercase();
            if name.contains("msedgewebview2.exe") && pe.th32ParentProcessID == browser_pid && pe.th32ProcessID != browser_pid {
                target_pid = Some(pe.th32ProcessID);
                break;
            }
            if Process32NextW(snap, &mut pe).is_err() { break; }
        }
        CloseHandle(snap).ok();

        let pid = target_pid.ok_or_else(|| Error::from(E_FAIL))?;
        let hproc = OpenProcess(PROCESS_ALL_ACCESS, false, pid)?;
        let dll_utf16: Vec<u16> = dll_path.encode_utf16().chain(Some(0)).collect();
        let bytes = (dll_utf16.len() * 2) as usize;

        let remote_mem = VirtualAllocEx(hproc, None, bytes, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE);
        if remote_mem.is_null() { return Err(Error::from(E_OUTOFMEMORY)); }
        WriteProcessMemory(hproc, remote_mem, dll_utf16.as_ptr() as _, bytes, None)?;

        let h_kernel32 = GetModuleHandleW(w!("kernel32.dll"))?;
        let loadlib = GetProcAddress(h_kernel32, s!("LoadLibraryW")).ok_or_else(|| Error::from(E_POINTER))?;
        let h_thread = CreateRemoteThread(hproc, None, 0, Some(std::mem::transmute(loadlib)), Some(remote_mem), 0, None)?;
        WaitForSingleObject(h_thread, INFINITE);

        CloseHandle(h_thread).ok();
        CloseHandle(hproc).ok();
        Ok(())
    }
}
