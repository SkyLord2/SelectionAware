use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::System::Com::*,
    Win32::System::Ole::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::System::Memory::*,
    Win32::System::DataExchange::*, // 包含剪贴板相关常量和函数
    Win32::UI::Input::KeyboardAndMouse::*, 
    Win32::UI::Accessibility::*, 
    Win32::UI::WindowsAndMessaging::*,
};

static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);
const SELECTION_THRESHOLD_MS: u64 = 200;

fn main() -> Result<()> {
    unsafe {
        // 1. 初始化 COM
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);

        // 2. 安装钩子
        let instance = GetModuleHandleW(None)?;
        let hook_id = SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook_proc),
            Some(instance.into()),
            0,
        )?;

        if hook_id.is_invalid() {
            eprintln!("无法安装鼠标钩子！");
            return Ok(());
        }

        println!("系统监控已启动...");
        println!("支持备份类型: 文本、文件(复制的文件)、图片(截图/位图)");

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hook_id);
    }
    Ok(())
}

unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        match wparam.0 as u32 {
            WM_LBUTTONDOWN => {
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                MOUSE_DOWN_TIME.store(now, Ordering::SeqCst);
            }
            WM_LBUTTONUP => {
                let start_time = MOUSE_DOWN_TIME.swap(0, Ordering::SeqCst);
                if start_time > 0 {
                    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                    let duration = now.saturating_sub(start_time);

                    if duration >= SELECTION_THRESHOLD_MS {
                        thread::spawn(move || {
                            thread::sleep(Duration::from_millis(100));
                            perform_smart_detection(duration);
                        });
                    }
                }
            }
            _ => {}
        }
    }
    // 显式 unsafe 块
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn perform_smart_detection(duration_ms: u64) {
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {}
        println!("\n[触发] 拖拽时长: {}ms", duration_ms);

        // 尝试 UIA
        if let Ok(text) = try_get_uia_text_deep() {
            if !text.trim().is_empty() {
                println!("✅ [UIA 成功]: {}", text);
                CoUninitialize();
                return;
            }
        }
        
        println!("⚠️ [UIA 失败] 启动剪贴板全量备份方案...");
        perform_clipboard_fallback();
        CoUninitialize();
    }
}

// =============================================================================
// Level 1: UIA Deep Dive
// =============================================================================
unsafe fn try_get_uia_text_deep() -> Result<String> {
    // 即使在 unsafe fn 中，调用 unsafe 函数也建议用 unsafe 块包裹
    let uia: IUIAutomation = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;
    let focused = unsafe { uia.GetFocusedElement() }?; 
    
    // 简化的 UIA 逻辑，优先 TextPattern
    if let Ok(pattern) = unsafe { focused.GetCurrentPattern(UIA_TextPatternId) } {
        let text_pattern: IUIAutomationTextPattern = pattern.cast()?;
        if let Ok(selection) = unsafe { text_pattern.GetSelection() } {
            if unsafe { selection.Length() }? > 0 {
                let item = unsafe { selection.GetElement(0) }?;
                return Ok(unsafe { item.GetText(-1) }?.to_string());
            }
        }
    }
    Ok(String::new())
}

// =============================================================================
// Level 2: 全能剪贴板备份与恢复
// =============================================================================
fn perform_clipboard_fallback() {
    // 1. 创建守卫：这会自动把当前的文本、文件、图片都读入内存
    let _guard = ClipboardGuard::new(); 

    // 2. 模拟 Ctrl + C
    // simulate_ctrl_c 本身被标记为 unsafe，这里需要 unsafe 块
    if unsafe { simulate_ctrl_c().is_err() } {
        println!("❌ [Fallback] 模拟按键失败");
        return;
    }

    // 3. 等待数据写入
    thread::sleep(Duration::from_millis(200));

    // 4. 读取新获取的文本 (只读文本用于 AI 分析)
    // get_clipboard_text_only 本身是 unsafe fn
    match unsafe { get_clipboard_text_only() } {
        Ok(text) if !text.trim().is_empty() => {
            println!("✅ [Fallback 成功]: {}", text);
        }
        _ => println!("❌ [Fallback 失败] 剪贴板无文本"),
    }
    
    // 5. 函数结束，_guard 自动 Drop
}

// --- 剪贴板数据结构 ---
struct ClipboardEntry {
    format: u32,
    data: Vec<u8>, // 原始二进制数据
}

struct ClipboardGuard {
    backups: Vec<ClipboardEntry>,
}

impl ClipboardGuard {
    fn new() -> Self {
        let backups = unsafe { Self::backup_clipboard() };
        if !backups.is_empty() {
            println!("💾 已备份剪贴板内容 ({} 种格式)", backups.len());
        }
        Self { backups }
    }

    // 备份核心逻辑
    unsafe fn backup_clipboard() -> Vec<ClipboardEntry> {
        let mut entries = Vec::new();
        let mut retry = 0;
        
        // 修复：OpenClipboard 是 unsafe，需要包裹
        while unsafe { OpenClipboard(None).is_err() } {
            retry += 1;
            if retry > 10 { return entries; }
            thread::sleep(Duration::from_millis(10));
        }

        // 遍历所有格式
        let mut format = 0;
        loop {
            // 修复：EnumClipboardFormats 是 unsafe
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 { break; }

            // --- 白名单过滤 ---
            let should_backup = matches!(format, 
                13 /*CF_UNICODETEXT*/ | 
                15 /*CF_HDROP*/ | 
                8  /*CF_DIB*/ | 
                17 /*CF_DIBV5*/
            );

            if should_backup {
                // 修复：GetClipboardData 是 unsafe
                if let Ok(handle) = unsafe { GetClipboardData(format) } {
                    if !handle.is_invalid() {
                        let h_global = HGLOBAL(handle.0);
                        // 修复：GlobalLock 是 unsafe
                        let ptr = unsafe { GlobalLock(h_global) };
                        if !ptr.is_null() {
                            // 修复：GlobalSize 是 unsafe
                            let size = unsafe { GlobalSize(h_global) };
                            if size > 0 {
                                let mut data = vec![0u8; size];
                                // 修复：copy_nonoverlapping 是 unsafe
                                unsafe { std::ptr::copy_nonoverlapping(ptr as *const u8, data.as_mut_ptr(), size) };
                                entries.push(ClipboardEntry { format, data });
                            }
                            // 修复：GlobalUnlock 是 unsafe
                            let _ = unsafe { GlobalUnlock(h_global) };
                        }
                    }
                }
            }
        }
        
        // 修复：CloseClipboard 是 unsafe，且忽略结果
        let _ = unsafe { CloseClipboard() };
        entries
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if self.backups.is_empty() { return; }
        println!("♻️ 尝试恢复剪贴板内容...");
        // 尝试恢复
        for _ in 0..10 {
            // 修复：OpenClipboard 是 unsafe
            if unsafe { OpenClipboard(None).is_ok() } {
                // 修复：EmptyClipboard 是 unsafe，且忽略结果
                let _ = unsafe { EmptyClipboard() };

                for entry in &self.backups {
                    // 修复：GlobalAlloc 是 unsafe
                    if let Ok(h_mem) = unsafe { GlobalAlloc(GMEM_MOVEABLE, entry.data.len()) } {
                        // 修复：GlobalLock 是 unsafe
                        let ptr = unsafe { GlobalLock(h_mem) };
                        if !ptr.is_null() {
                            // 修复：copy_nonoverlapping 是 unsafe
                            unsafe { std::ptr::copy_nonoverlapping(entry.data.as_ptr(), ptr as *mut u8, entry.data.len()) };
                            // 修复：GlobalUnlock 是 unsafe
                            let _ = unsafe { GlobalUnlock(h_mem) };
                            
                            // 设置剪贴板数据
                            // 修复：SetClipboardData 是 unsafe
                            if unsafe { SetClipboardData(entry.format, Some(HANDLE(h_mem.0))).is_err() } {
                                // 修复：GlobalFree 是 unsafe，且忽略结果
                                let _ = unsafe { GlobalFree(Some(h_mem)) };
                            }
                        } else {
                            let _ = unsafe { GlobalFree(Some(h_mem)) };
                        }
                    }
                }
                // 修复：CloseClipboard 是 unsafe，且忽略结果
                let _ = unsafe { CloseClipboard() };
                println!("♻️ 剪贴板已还原 (文件/图片/文本)");
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

// --- 辅助函数：只获取当前剪贴板文本 (用于业务逻辑) ---
unsafe fn get_clipboard_text_only() -> Result<String> {
    let mut retry = 0;
    while unsafe { OpenClipboard(None).is_err() } {
        retry += 1;
        // 修复：GetLastError 是 unsafe
        if retry > 10 { return Err(Error::from(unsafe { GetLastError() })); }
        thread::sleep(Duration::from_millis(10));
    }

    let result = (|| -> Result<String> {
        // 修复：IsClipboardFormatAvailable 是 unsafe
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() } {
            return Ok(String::new());
        }
        // 修复：GetClipboardData 是 unsafe
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32) }?;
        if handle.is_invalid() { return Ok(String::new()); }
        
        let h_global = HGLOBAL(handle.0);
        // 修复：GlobalLock 是 unsafe
        let ptr = unsafe { GlobalLock(h_global) };
        if ptr.is_null() { return Ok(String::new()); }
        
        // 修复：offset 是 unsafe
        let len = (0..).take_while(|&i| unsafe { *ptr.cast::<u16>().offset(i) != 0 }).count();
        // 修复：from_raw_parts 是 unsafe
        let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len) };
        let text = String::from_utf16_lossy(slice);
        
        // 修复：GlobalUnlock 是 unsafe，且忽略结果
        let _ = unsafe { GlobalUnlock(h_global) };
        Ok(text)
    })();

    // 修复：CloseClipboard 是 unsafe，且忽略结果
    let _ = unsafe { CloseClipboard() };
    result
}

unsafe fn simulate_ctrl_c() -> Result<()> {
    let inputs = [
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_CONTROL, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_C, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_C, dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_CONTROL, dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
    ];
    // 修复：SendInput 是 unsafe
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    Ok(())
}