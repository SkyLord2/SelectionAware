use std::ffi::c_void;
use windows::{
    core::*,
    Win32::Graphics::Gdi::*,
    Graphics::Imaging::{BitmapPixelFormat, BitmapAlphaMode, SoftwareBitmap},
    Media::Ocr::OcrEngine,
    Storage::Streams::DataWriter,
};

// 矩形结构体
#[derive(Clone, Copy, Debug)]
pub struct ScreenRect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl ScreenRect {
    pub fn width(&self) -> i32 { self.right - self.left }
    pub fn height(&self) -> i32 { self.bottom - self.top }
}

/// 执行屏幕截图并进行 OCR 识别
pub async fn capture_and_recognize(rect: ScreenRect) -> Result<String> {
    // 1. GDI 截屏：获取像素数据
    let (bytes, width, height) = unsafe { capture_screen_area(rect)? };
    
    if bytes.is_empty() {
        println!("截图失败或区域无效");
        return Ok(String::new());
    }
    println!("截图成功: {}x{}, 共 {} 字节", width, height, bytes.len());
    // 2. 将像素数据转换为 WinRT SoftwareBitmap
    let software_bitmap = create_software_bitmap_from_bytes(&bytes, width, height)?;

    // 3. 初始化 OCR 引擎
    // TryCreateFromUserProfileLanguages 会使用用户系统的首选语言（如中文/英文）
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()?;
    
    // 4. 执行识别 (这是一个异步操作)
    let result = engine.RecognizeAsync(&software_bitmap)?.await?;
    
    // 5. 提取文本
    let lines = result.Lines()?;
    let mut full_text = String::new();
    
    for line in lines {
        full_text.push_str(&line.Text()?.to_string());
        full_text.push('\n'); // 换行
    }

    Ok(full_text.trim().to_string())
}

/// Win32 GDI 截图逻辑
unsafe fn capture_screen_area(rect: ScreenRect) -> Result<(Vec<u8>, i32, i32)> {
    let w = rect.width();
    let h = rect.height();
    
    if w <= 0 || h <= 0 {
        return Ok((vec![], 0, 0));
    }

    // 获取屏幕 DC
    let hdc_screen = unsafe { GetDC(None) };
    // 创建内存 DC
    let hdc_mem = unsafe { CreateCompatibleDC(Some(hdc_screen)) };
    // 创建兼容位图
    let hbitmap = unsafe { CreateCompatibleBitmap(hdc_screen, w, h) };
    
    // 将位图选入内存 DC
    // 【修复点 1】需要将 HBITMAP 转换为 HGDIOBJ
    let hgdiobj: HGDIOBJ = hbitmap.into();
    let old_obj = unsafe { SelectObject(hdc_mem, hgdiobj) };

    // 将屏幕内容复制到内存 DC (BitBlt)
    // SRCCOPY = 0x00CC0020
    (unsafe { BitBlt(hdc_mem, 0, 0, w, h, Some(hdc_screen), rect.left, rect.top, SRCCOPY) })?;

    // 准备提取像素数据
    let mut bmi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // 负数表示从上到下的行顺序
            biPlanes: 1,
            biBitCount: 32, // BGRA 格式
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };

    let mut pixels: Vec<u8> = vec![0; (w * h * 4) as usize];
    
    // 获取具体像素位
    unsafe { GetDIBits(
        hdc_mem,
        hbitmap,
        0,
        h as u32,
        Some(pixels.as_mut_ptr() as *mut c_void),
        &mut bmi,
        DIB_RGB_COLORS,
    ) };

    // 清理资源
    // 【修复点 2】清理时同样需要转换类型
    unsafe { SelectObject(hdc_mem, old_obj) };
    unsafe { let _ = DeleteObject(hgdiobj); }; // 删除我们创建的 hbitmap
    unsafe { let _ = DeleteDC(hdc_mem); };
    unsafe { ReleaseDC(None, hdc_screen) };

    Ok((pixels, w, h))
}

/// 辅助函数：将字节数组转换为 WinRT SoftwareBitmap
fn create_software_bitmap_from_bytes(pixels: &[u8], width: i32, height: i32) -> Result<SoftwareBitmap> {
    // 【修复点 3】Create 方法只有 3 个参数，带 Alpha 的版本叫 CreateWithAlpha
    let bitmap = SoftwareBitmap::CreateWithAlpha(
        BitmapPixelFormat::Bgra8,
        width,
        height,
        BitmapAlphaMode::Premultiplied,
    )?;

    // 将数据写入 Bitmap buffer
    // let buffer = bitmap.LockBuffer(windows::Graphics::Imaging::BitmapBufferAccessMode::Write)?;
    // 注意：这里其实不需要 CreateReference，可以直接操作 DataWriter
    // let reference = buffer.CreateReference()?; 
    
    let writer = DataWriter::new()?;
    writer.WriteBytes(pixels)?;
    
    // 这里使用 DetachBuffer 获取 IBuffer 并写入 SoftwareBitmap
    let ibuffer = writer.DetachBuffer()?;
    bitmap.CopyFromBuffer(&ibuffer)?;
    
    Ok(bitmap)
}