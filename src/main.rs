use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::System::Com::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::UI::Accessibility::*,
    Win32::UI::WindowsAndMessaging::*,
};

// 全局原子变量，用于记录鼠标左键按下的时间戳（毫秒）
// 0 表示未按下
static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);

// 定义“长按/拖拽”的阈值 (毫秒)
// 如果按下到抬起的时间小于这个值，被视为普通点击，不触发识别
const SELECTION_THRESHOLD_MS: u64 = 200;

fn main() -> Result<()> {
    unsafe {
        // 1. 设置全局鼠标钩子
        let instance = GetModuleHandleW(None)?;
        let instance_handle = HINSTANCE(instance.0);
        let hook_id = SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(mouse_hook_proc),
            Some(instance_handle),
            0,
        )?;

        if hook_id.is_invalid() {
            eprintln!("无法安装鼠标钩子！");
            return Ok(());
        }

        println!("系统监控已启动...");
        println!("请尝试：按住鼠标左键 -> 拖拽选中文字 -> 松开鼠标");
        println!("(短按点击不会触发)");

        // 2. 开启 Windows 消息循环 (必须，否则钩子不生效)
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).into() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 退出前卸载钩子
        let _ = UnhookWindowsHookEx(hook_id);
    }
    Ok(())
}

// -----------------------------------------------------------------------------
// 鼠标钩子回调函数 (必须是 extern "system")
// -----------------------------------------------------------------------------
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // 如果 code < 0，必须直接透传给下一个钩子
    if code >= 0 {
        let msg = wparam.0 as u32;

        match msg {
            WM_LBUTTONDOWN => {
                // 记录按下时间
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                MOUSE_DOWN_TIME.store(now, Ordering::SeqCst);
            }
            WM_LBUTTONUP => {
                // 获取按下时的存储时间
                let start_time = MOUSE_DOWN_TIME.swap(0, Ordering::SeqCst);
                
                if start_time > 0 {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;

                    // 计算持续时间
                    let duration = now.saturating_sub(start_time);

                    // 只有当持续时间超过阈值（说明可能是拖拽选区操作）时，才触发识别
                    if duration >= SELECTION_THRESHOLD_MS {
                        // 【关键】不要在钩子回调里做耗时操作，开启新线程处理
                        thread::spawn(move || {
                            // 给 UI 一点时间完成渲染和内部状态更新
                            thread::sleep(Duration::from_millis(50));
                            perform_uia_detection(duration);
                        });
                    }
                }
            }
            _ => {}
        }
    }

    // 必须调用 CallNextHookEx 让其他软件也能收到鼠标消息
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

// -----------------------------------------------------------------------------
// UIA 识别逻辑 (运行在独立线程中)
// -----------------------------------------------------------------------------
fn perform_uia_detection(duration_ms: u64) {
    // 这里的 COM 初始化和 UIA 逻辑与之前完全一致
    // 注意：CoInitializeEx 必须在当前线程调用
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }

        // 尝试获取选中文本
        match get_focused_selection() {
            Ok(text) => {
                if !text.trim().is_empty() {
                    println!("--------------------------------------------------");
                    println!("检测到长按/拖拽 ({}ms) 结束，捕获文本:", duration_ms);
                    println!(">>> {}", text);
                    println!("--------------------------------------------------");
                }
            }
            Err(_) => {
                // 忽略未选中或不支持的情况
            }
        }
        
        // 线程结束前自动清理 COM，Rust RAII 会处理局部变量，但 CoUninitialize 需要手动吗？
        // Windows crate 的 CoInitializeEx 通常不需要显式 Uninitialize，除非极严谨的 COM 编程
        // 这里简化处理
    }
}

// 复用之前的 UIA 获取逻辑
fn get_focused_selection() -> Result<String> {
    unsafe {
        let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let focused_element = uia.GetFocusedElement()?;
        
        // 尝试获取 TextPattern
        let pattern_obj = focused_element.GetCurrentPattern(UIA_TextPatternId)?;
        let text_pattern: IUIAutomationTextPattern = match pattern_obj.cast() {
            Ok(p) => p,
            Err(_) => return Ok(String::new()),
        };

        let selection_ranges = text_pattern.GetSelection()?;
        let count = selection_ranges.Length()?;

        if count == 0 {
            return Ok(String::new());
        }

        let mut full_text = String::new();
        for i in 0..count {
            let range = selection_ranges.GetElement(i)?;
            let text_bstr = range.GetText(-1)?;
            full_text.push_str(&text_bstr.to_string());
        }

        Ok(full_text)
    }
}