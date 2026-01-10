mod ocr;

use std::sync::atomic::{AtomicU64, AtomicI32, Ordering};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};
use windows::{
    core::*,
    Win32::Foundation::*,
    Win32::System::Com::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::UI::Accessibility::*,
    Win32::UI::WindowsAndMessaging::*,
    Win32::UI::HiDpi::*,
};
use crate::ocr::{ScreenRect, capture_and_recognize};

// 用于存储坐标 (简单起见使用原子变量)
static START_X: AtomicI32 = AtomicI32::new(0);
static START_Y: AtomicI32 = AtomicI32::new(0);

// 全局原子变量，用于记录鼠标左键按下的时间戳（毫秒）
// 0 表示未按下
static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);

// 定义“长按/拖拽”的阈值 (毫秒)
// 如果按下到抬起的时间小于这个值，被视为普通点击，不触发识别
const SELECTION_THRESHOLD_MS: u64 = 200;

fn main() -> Result<()> {
    unsafe {
        // 告诉 Windows：我是高 DPI 感知的，不要对我进行虚拟化缩放
        // 这样 GetSystemMetrics 和鼠标坐标都会返回物理像素值
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
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

                // 记录坐标 (low-level 钩子消息包含坐标)
                let ms_struct = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
                START_X.store(ms_struct.pt.x, Ordering::SeqCst);
                START_Y.store(ms_struct.pt.y, Ordering::SeqCst);
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
                        // 获取结束坐标
                        let ms_struct = unsafe { *(lparam.0 as *const MSLLHOOKSTRUCT) };
                        let end_x = ms_struct.pt.x;
                        let end_y = ms_struct.pt.y;
                        
                        let start_x = START_X.load(Ordering::SeqCst);
                        let start_y = START_Y.load(Ordering::SeqCst);

                        // 计算原始矩形
                        let mut top = start_y.min(end_y);
                        let mut bottom = start_y.max(end_y);
                        let left = start_x.min(end_x);
                        let right = start_x.max(end_x);

                        // ==========================================
                        // 【核心修复】智能高度修正
                        // ==========================================
                        let height = bottom - top;
                        // 如果高度小于 20px，说明用户只是水平划词，没有框选整行
                        // 我们强制将高度扩展到 40px (上下各扩展一些)，以确保截取到完整的文字
                        if height < 20 {
                            let center_y = (top + bottom) / 2;
                            // 假设文字高度大概是 30-40px，上下各扩充 20px
                            top = center_y - 20;
                            bottom = center_y + 20;
                            
                            // 简单的边界检查 (防止负坐标，虽然 BitBlt 通常能处理)
                            if top < 0 { top = 0; }
                        }

                        // 构造最终的 Rect
                        let rect = ScreenRect {
                            left,
                            top,
                            right,
                            bottom,
                        };

                        // 打印日志方便调试确认
                        println!("选区修正: 原始高度 {} -> 修正后高度 {}", height, rect.height());

                        // 【关键】不要在钩子回调里做耗时操作，开启新线程处理
                        thread::spawn(move || {
                            let rt = tokio::runtime::Runtime::new().unwrap();
                            rt.block_on(async {
                                match perform_smart_detection(duration, rect).await {
                                    Ok(_) => {},
                                    Err(e) => println!("识别出错: {:?}", e),
                                }
                            });
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
async fn perform_smart_detection(_duration_ms: u64, rect: ScreenRect) -> Result<()> {
    // 这里的 COM 初始化和 UIA 逻辑与之前完全一致
    // 注意：CoInitializeEx 必须在当前线程调用
    println!("启动 UIA 识别线程...");
    unsafe {
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            println!("无法初始化 COM!");
            CoUninitialize(); // 退出前清理
            return Ok(());
        }

        // 尝试获取选中文本
        // 1. 先尝试 UIA (如果非 Canvas 网页，UIA更快更准)
        // 注意：get_focused_selection 是同步代码，可以直接调
        if let Ok(text) = get_focused_selection() {
            if !text.is_empty() {
                println!("UIA 成功: {}", text);
                return Ok(());
            }
        }
        
        let ocr_text = capture_and_recognize(rect).await?;
        if !ocr_text.trim().is_empty() {
            println!("--------------------------------------------------");
            println!("OCR 识别结果:");
            println!("{}", ocr_text);
            println!("--------------------------------------------------");
        } else {
            println!("OCR 未识别到文字");
        }
        // 线程结束前自动清理 COM，Rust RAII 会处理局部变量，但 CoUninitialize 需要手动吗？
        // Windows crate 的 CoInitializeEx 通常不需要显式 Uninitialize，除非极严谨的 COM 编程
        // 这里简化处理
        Ok(())
    }
}

// 复用之前的 UIA 获取逻辑
fn get_focused_selection() -> Result<String> {
    println!("正在获取选中文本...");
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
            println!("未选中任何文本");
            return Ok(String::new());
        }
        println!("共 {} 个选中文本", count);
        let mut full_text = String::new();
        for i in 0..count {
            let range = selection_ranges.GetElement(i)?;
            let text_bstr = range.GetText(-1)?;
            full_text.push_str(&text_bstr.to_string());
        }

        Ok(full_text)
    }
}