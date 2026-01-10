
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
    Win32::System::DataExchange::*, // 包含 CF_UNICODETEXT, OpenClipboard 等
    Win32::UI::Input::KeyboardAndMouse::*, 
    Win32::UI::Accessibility::*, 
    Win32::UI::WindowsAndMessaging::*,
};

static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);
const SELECTION_THRESHOLD_MS: u64 = 200;

fn main() -> Result<()> {
    unsafe {
        // 1. 初始化 COM (主线程)
        // 忽略初始化错误（可能是已经初始化过了）
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
        println!("策略: UIA 优先 -> 失败则回退到 Ctrl+C (带剪贴板保护)");

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
                        // 开启新线程处理，避免阻塞钩子
                        thread::spawn(move || {
                            // 等待 UI 渲染/选区稳定
                            thread::sleep(Duration::from_millis(100));
                            perform_smart_detection(duration);
                        });
                    }
                }
            }
            _ => {}
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// =============================================================================
// 核心业务逻辑
// =============================================================================
fn perform_smart_detection(duration_ms: u64) {
    unsafe {
        // 线程内初始化 COM
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            // 如果失败通常是因为线程已初始化，继续尝试
        }

        println!("\n[触发] 拖拽时长: {}ms", duration_ms);

        // --- 阶段一：尝试 UIA (无感、高性能) ---
        match try_get_uia_text_deep() {
            Ok(text) if !text.trim().is_empty() => {
                println!("✅ [UIA 成功]: {}", text);
            }
            _ => {
                println!("⚠️ [UIA 失败] 无法获取有效文本，启动 Level 2 降级方案...");
                
                // --- 阶段二：剪贴板回退 (模拟 Ctrl+C) ---
                // 使用 Guard 保护原有剪贴板内容
                perform_clipboard_fallback();
            }
        }

        CoUninitialize();
    }
}

// =============================================================================
// Level 1: UIA Deep Dive (深层探测)
// =============================================================================
unsafe fn try_get_uia_text_deep() -> Result<String> {
    // 显式 unsafe 块处理 unsafe 调用
    let uia: IUIAutomation = unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }?;
    let focused = unsafe { uia.GetFocusedElement() }?; // 获取当前焦点元素

    // 1. 尝试直接从焦点元素获取
    if let Ok(text) = unsafe { extract_text_from_element(&focused) } {
        if !text.trim().is_empty() { return Ok(text); }
    }

    // 2. 如果焦点元素是容器 (Group/Pane/Document)，尝试遍历其子元素
    let condition = unsafe { uia.CreateTrueCondition() }?; // 匹配所有子节点
    if let Ok(children) = unsafe { focused.FindAll(TreeScope_Descendants, &condition) } {
        let count = unsafe { children.Length() }?;
        // 限制遍历数量防止卡顿
        let limit = count.min(20); 
        
        for i in 0..limit {
            if let Ok(child) = unsafe { children.GetElement(i) } {
                if let Ok(text) = unsafe { extract_text_from_element(&child) } {
                    if !text.trim().is_empty() {
                        return Ok(text);
                    }
                }
            }
        }
    }

    Ok(String::new())
}

// 从单个 UIA 节点提取文本的辅助函数
unsafe fn extract_text_from_element(element: &IUIAutomationElement) -> Result<String> {
    // 优先级 A: TextPattern (标准选区)
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_TextPatternId) } {
        let text_pattern: IUIAutomationTextPattern = pattern.cast()?;
        if let Ok(selection) = unsafe { text_pattern.GetSelection() } {
            if unsafe { selection.Length() }? > 0 {
                let item = unsafe { selection.GetElement(0) }?;
                return Ok(unsafe { item.GetText(-1) }?.to_string());
            }
        }
    }

    // 优先级 B: ValuePattern (输入框值)
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_ValuePatternId) } {
        let value_pattern: IUIAutomationValuePattern = pattern.cast()?;
        return Ok(unsafe { value_pattern.CurrentValue() }?.to_string());
    }

    // 优先级 C: LegacyIAccessiblePattern (兼容模式)
    if let Ok(pattern) = unsafe { element.GetCurrentPattern(UIA_LegacyIAccessiblePatternId) } {
        let legacy: IUIAutomationLegacyIAccessiblePattern = pattern.cast()?;
        // 尝试 Value
        let val = unsafe { legacy.CurrentValue() }?.to_string();
        if !val.trim().is_empty() { return Ok(val); }
        // 尝试 Name
        let name = unsafe { legacy.CurrentName() }?.to_string();
        if !name.trim().is_empty() { return Ok(name); }
    }

    Ok(String::new())
}

// =============================================================================
// Level 2: Clipboard Fallback (剪贴板回退方案)
// =============================================================================
fn perform_clipboard_fallback() {
    let _guard = ClipboardGuard::new(); // 1. 自动备份剪贴板

    unsafe {
        // 2. 模拟 Ctrl + C
        if simulate_ctrl_c().is_err() {
            println!("❌ [Fallback] 模拟按键失败");
            return;
        }

        // 3. 等待系统写入剪贴板
        thread::sleep(Duration::from_millis(200));

        // 4. 读取新内容
        match get_clipboard_text() {
            Ok(text) if !text.trim().is_empty() => {
                println!("✅ [Fallback 成功]: {}", text);
            }
            _ => println!("❌ [Fallback 失败] 剪贴板无文本"),
        }
    }
}

// --- 剪贴板保护守卫 (RAII) ---
struct ClipboardGuard {
    original_text: Option<String>,
}

impl ClipboardGuard {
    fn new() -> Self {
        // 尝试保存当前文本
        let original_text = unsafe { get_clipboard_text().ok() }.filter(|s| !s.is_empty());
        Self { original_text }
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        if let Some(text) = &self.original_text {
            unsafe {
                // 重试写回
                for _ in 0..5 {
                    // OpenClipboard 返回 Result<()>，直接 .is_ok() 判断成功
                    if OpenClipboard(None).is_ok() {
                        let _ = EmptyClipboard();
                        
                        let wide: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
                        // GlobalAlloc 返回 Result<HGLOBAL>
                        if let Ok(h_mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
                            let ptr = GlobalLock(h_mem);
                            if !ptr.is_null() {
                                std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr as *mut u16, wide.len());
                                let _ = GlobalUnlock(h_mem);
                                
                                // SetClipboardData 需要 (u32, Option<HANDLE>)
                                // 我们需要把 HGLOBAL 转换为 HANDLE
                                let handle = HANDLE(h_mem.0); 
                                let _ = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(handle));
                            }
                        }
                        let _ = CloseClipboard();
                        break;
                    }
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }
}

// --- 底层辅助函数 ---

unsafe fn simulate_ctrl_c() -> Result<()> {
    let inputs = [
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_CONTROL, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_C, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_C, dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
        INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: VK_CONTROL, dwFlags: KEYEVENTF_KEYUP, ..Default::default() } } },
    ];
    unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    Ok(())
}

unsafe fn get_clipboard_text() -> Result<String> {
    let mut retry = 0;
    // OpenClipboard 返回 Result<()>，不能用 as_bool()
    while unsafe { OpenClipboard(None).is_err() } {
        retry += 1;
        if retry > 10 { return Err(Error::from(unsafe { GetLastError() })); }
        thread::sleep(Duration::from_millis(10));
    }

    let result = (|| -> Result<String> {
        // IsClipboardFormatAvailable 返回 Result<()>，表示可用
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32) }.is_err()  {
            return Ok(String::new());
        }

        // GetClipboardData 返回 Result<HANDLE>
        let handle = unsafe { GetClipboardData(CF_UNICODETEXT.0 as u32)? };
        if handle.is_invalid() { return Ok(String::new()); }
        
        // GlobalLock 需要 HGLOBAL，显式转换
        let h_global = HGLOBAL(handle.0);
        let ptr = unsafe { GlobalLock(h_global) };
        
        if ptr.is_null() { return Ok(String::new()); }
        
        let len = (0..).take_while(|&i| unsafe { *ptr.cast::<u16>().offset(i) } != 0).count();
        let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u16>(), len) };
        let text = String::from_utf16_lossy(slice);
        
        let _ = unsafe { GlobalUnlock(h_global) };
        Ok(text)
    })();

    let _ = unsafe { CloseClipboard() };
    result
}