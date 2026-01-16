use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};
use std::sync::{OnceLock, mpsc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use windows::{
    Win32::Foundation::*,
    Win32::System::Com::*,
    Win32::System::DataExchange::*,
    Win32::System::LibraryLoader::GetModuleHandleW,
    Win32::System::Memory::*,
    Win32::System::Ole::{CLIPBOARD_FORMAT, OleDuplicateData},
    Win32::UI::Accessibility::*,
    Win32::UI::Input::KeyboardAndMouse::*,
    Win32::UI::WindowsAndMessaging::*,
    core::*,
};

// 全局原子变量，用于记录鼠标左键按下的时间戳（毫秒）
// 0 表示未按下
static MOUSE_DOWN_TIME: AtomicU64 = AtomicU64::new(0);
static HOOK_HANDLE: AtomicPtr<std::ffi::c_void> = AtomicPtr::new(std::ptr::null_mut());
static WORKER_TX: OnceLock<mpsc::Sender<u64>> = OnceLock::new();

// 定义“长按/拖拽”的阈值 (毫秒)
// 如果按下到抬起的时间小于这个值，被视为普通点击，不触发识别
const SELECTION_THRESHOLD_MS: u64 = 200;
const CF_UNICODETEXT_U32: u32 = 13;

fn main() -> Result<()> {
    let (tx, rx) = mpsc::channel::<u64>();
    let _ = WORKER_TX.set(tx);
    let _ = thread::Builder::new()
        .name("uia-worker".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(move || worker_loop(rx));

    unsafe {
        // 1. 设置全局鼠标钩子
        let instance = GetModuleHandleW(None)?;
        let instance_handle = HINSTANCE(instance.0);
        let hook_id =
            SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), Some(instance_handle), 0)?;

        if hook_id.is_invalid() {
            eprintln!("无法安装鼠标钩子！");
            return Ok(());
        }
        HOOK_HANDLE.store(hook_id.0, Ordering::SeqCst);

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
        HOOK_HANDLE.store(std::ptr::null_mut(), Ordering::SeqCst);
        let _ = UnhookWindowsHookEx(hook_id);
    }
    Ok(())
}

fn worker_loop(rx: mpsc::Receiver<u64>) {
    unsafe {
        // UIAutomation 客户端建议在 MTA 中使用；STA 在部分场景下可能触发深层重入导致栈溢出。
        if CoInitializeEx(None, COINIT_MULTITHREADED).is_err() {
            return;
        }
    }

    while let Ok(duration_ms) = rx.recv() {
        // 给目标应用一点时间完成选区状态更新，避免过早读取到空选区。
        thread::sleep(Duration::from_millis(50));
        perform_uia_detection(duration_ms);
    }

    unsafe {
        CoUninitialize();
    }
}

// -----------------------------------------------------------------------------
// 鼠标钩子回调函数 (必须是 extern "system")
// -----------------------------------------------------------------------------
unsafe extern "system" fn mouse_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let msg = wparam.0 as u32;

        match msg {
            WM_LBUTTONDOWN => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                MOUSE_DOWN_TIME.store(now, Ordering::SeqCst);
            }
            WM_LBUTTONUP => {
                let start_time = MOUSE_DOWN_TIME.swap(0, Ordering::SeqCst);
                if start_time > 0 {
                    let now = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    let duration = now.saturating_sub(start_time);
                    if duration >= SELECTION_THRESHOLD_MS
                        && let Some(tx) = WORKER_TX.get()
                    {
                        // 钩子回调里不要做耗时/COM/剪贴板操作：只投递事件给 worker 线程处理。
                        let _ = tx.send(duration);
                    }
                }
            }
            _ => {}
        }
    }

    let hook = HOOK_HANDLE.load(Ordering::SeqCst);
    let hook = if hook.is_null() {
        None
    } else {
        Some(HHOOK(hook))
    };
    unsafe { CallNextHookEx(hook, code, wparam, lparam) }
}

// -----------------------------------------------------------------------------
// UIA 识别逻辑 (运行在独立线程中)
// -----------------------------------------------------------------------------
fn perform_uia_detection(duration_ms: u64) {
    if let Ok(text) = get_focused_selection_with_fallback_copy()
        && !text.trim().is_empty()
    {
        println!("--------------------------------------------------");
        println!("检测到长按/拖拽 ({}ms) 结束，捕获文本:", duration_ms);
        println!(">>> {}", text);
        println!("--------------------------------------------------");
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

fn get_focused_selection_with_fallback_copy() -> Result<String> {
    let text = get_focused_selection().unwrap_or_default();
    if !text.trim().is_empty() {
        return Ok(text);
    }
    unsafe { get_selection_via_copy_preserving_clipboard() }
}

struct ClipboardSnapshot {
    items: Vec<(u32, HANDLE)>,
}

impl ClipboardSnapshot {
    unsafe fn capture() -> Result<Self> {
        // 这里使用 Win32 剪贴板 API 做“数据级备份”：枚举所有格式并复制数据句柄。
        // 目的：避免 OleSetClipboard(IDataObject) 成为 clipboard owner 后影响图片等格式的后续粘贴。
        unsafe { OpenClipboard(None)? };
        let mut items: Vec<(u32, HANDLE)> = Vec::new();

        let mut format: u32 = 0;
        loop {
            format = unsafe { EnumClipboardFormats(format) };
            if format == 0 {
                let err = unsafe { GetLastError() };
                if err == ERROR_SUCCESS {
                    break;
                }
                let _ = unsafe { CloseClipboard() };
                // 这里返回线程最近一次 Win32 错误（GetLastError）。
                // rust-analyzer 在同名 HRESULT 类型上容易误判，因此避免手动拼 HRESULT。
                return Err(windows::core::Error::from_thread());
            }

            if let Ok(handle) = unsafe { GetClipboardData(format) } {
                // OleDuplicateData 会尝试把该格式的数据复制为可跨进程保存的句柄（多数常见格式有效）。
                // 对延迟渲染/私有格式可能复制失败，此时只是不记录该格式，不会中断流程。
                let dup = unsafe {
                    OleDuplicateData(
                        handle,
                        CLIPBOARD_FORMAT(format as u16),
                        GLOBAL_ALLOC_FLAGS(0),
                    )
                };
                if !dup.0.is_null() {
                    items.push((format, dup));
                }
            }
        }

        let _ = unsafe { CloseClipboard() };
        Ok(Self { items })
    }

    unsafe fn restore(self) {
        // 恢复时会 EmptyClipboard + SetClipboardData，因此剪贴板 owner 一定会变为当前进程/线程。
        // 但放回去的是“数据本体”，不会依赖本进程继续提供 IDataObject，从而避免图片粘贴丢失。
        let mut opened = false;
        for _ in 0..5 {
            if unsafe { OpenClipboard(None) }.is_ok() {
                opened = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }

        if !opened {
            return;
        }

        let _ = unsafe { EmptyClipboard() };

        for (format, handle) in self.items {
            let _ = unsafe { SetClipboardData(format, Some(handle)) };
        }

        let _ = unsafe { CloseClipboard() };
    }
}

unsafe fn get_selection_via_copy_preserving_clipboard() -> Result<String> {
    let snapshot = match unsafe { ClipboardSnapshot::capture() } {
        Ok(s) => s,
        Err(_) => return Ok(String::new()),
    };

    let original_seq = unsafe { GetClipboardSequenceNumber() };
    unsafe { send_ctrl_c()? };

    // 等待 Ctrl+C 触发目标应用写入剪贴板；用序列号变化作为信号避免盲等。
    for _ in 0..30 {
        if unsafe { GetClipboardSequenceNumber() } != original_seq {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    let copied_text = unsafe { read_clipboard_unicode_text() }.unwrap_or_default();
    // 无论是否读到文本都尝试恢复，尽量把剪贴板还原到触发前状态（图片等格式也包含在内）。
    unsafe { snapshot.restore() };
    Ok(copied_text)
}

unsafe fn send_ctrl_c() -> Result<()> {
    let inputs = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x43),
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0x43),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VK_CONTROL,
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];

    let sent = unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
    if sent != inputs.len() as u32 {
        return Err(windows::core::Error::from_thread());
    }
    Ok(())
}

unsafe fn read_clipboard_unicode_text() -> Result<String> {
    unsafe { OpenClipboard(None)? };

    let mut text = String::new();
    let handle = unsafe { GetClipboardData(CF_UNICODETEXT_U32)? };
    if !handle.0.is_null() {
        let hglobal = HGLOBAL(handle.0);
        let locked = unsafe { GlobalLock(hglobal) };
        if !locked.is_null() {
            let mut len = 0usize;
            let mut ptr = locked as *const u16;
            while unsafe { *ptr } != 0 {
                len += 1;
                ptr = unsafe { ptr.add(1) };
            }
            let slice = unsafe { std::slice::from_raw_parts(locked as *const u16, len) };
            text = String::from_utf16_lossy(slice);
            let _ = unsafe { GlobalUnlock(hglobal) };
        }
    }

    let _ = unsafe { CloseClipboard() };
    Ok(text)
}
