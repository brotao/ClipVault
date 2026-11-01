use std::borrow::Cow;
use std::io::Cursor;
use std::time::Duration;

use anyhow::{bail, Result};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use image::ImageFormat;
use tokio::sync::mpsc::UnboundedSender;

use crate::models::{DeviceInfo, HistoryEntry};
use crate::pairing::{now_ms, preview_text, sha256_hex};

const CLIPBOARD_RETRIES: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(30);

/// Reads text (preferred) or a PNG image from the clipboard.
///
/// Returns the change signature together with the history entry, or `None` when
/// the clipboard holds nothing we can sync.
pub fn read_clipboard(
    device: &DeviceInfo,
    has_peers: bool,
) -> Result<Option<(String, HistoryEntry)>> {
    let mut last_error = None;
    for attempt in 0..CLIPBOARD_RETRIES {
        match read_clipboard_once(device, has_peers) {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < CLIPBOARD_RETRIES {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("无法读取系统剪贴板")))
}

fn read_clipboard_once(
    device: &DeviceInfo,
    has_peers: bool,
) -> Result<Option<(String, HistoryEntry)>> {
    let mut clipboard = arboard::Clipboard::new().map_err(map_clipboard_error)?;

    match clipboard.get_text() {
        Ok(text) => {
            if !text.trim().is_empty() {
                let hash = sha256_hex(text.as_bytes());
                let entry = HistoryEntry {
                    id: uuid::Uuid::new_v4().to_string(),
                    mime_type: "text/plain".to_string(),
                    preview: preview_text(&text),
                    text: Some(text),
                    image_base64: None,
                    created_at: now_ms(),
                    source_device_id: device.id.clone(),
                    source_device_name: device.name.clone(),
                    sha256: hash.clone(),
                    direction: direction(has_peers),
                };
                return Ok(Some((format!("text:{hash}"), entry)));
            }
        }
        Err(arboard::Error::ContentNotAvailable) => {}
        Err(error) => return Err(map_clipboard_error(error)),
    }

    let image = match clipboard.get_image() {
        Ok(image) => image,
        Err(arboard::Error::ContentNotAvailable) => return Ok(None),
        Err(error) => return Err(map_clipboard_error(error)),
    };

    let Some(rgba) = image::RgbaImage::from_raw(
        image.width as u32,
        image.height as u32,
        image.bytes.into_owned(),
    ) else {
        return Ok(None);
    };

    let mut png = Vec::new();
    rgba.write_to(&mut Cursor::new(&mut png), ImageFormat::Png)?;
    if png.is_empty() {
        return Ok(None);
    }

    let hash = sha256_hex(&png);
    let entry = HistoryEntry {
        id: uuid::Uuid::new_v4().to_string(),
        mime_type: "image/png".to_string(),
        preview: "PNG 图片".to_string(),
        text: None,
        image_base64: Some(BASE64.encode(&png)),
        created_at: now_ms(),
        source_device_id: device.id.clone(),
        source_device_name: device.name.clone(),
        sha256: hash.clone(),
        direction: direction(has_peers),
    };

    Ok(Some((format!("image:{hash}"), entry)))
}

fn direction(has_peers: bool) -> String {
    if has_peers {
        "outbound".to_string()
    } else {
        "local".to_string()
    }
}

pub fn write_clipboard(entry: &HistoryEntry) -> Result<()> {
    let mut last_error = None;
    for attempt in 0..CLIPBOARD_RETRIES {
        match write_clipboard_once(entry) {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < CLIPBOARD_RETRIES {
                    std::thread::sleep(RETRY_DELAY);
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("无法写入系统剪贴板")))
}

fn write_clipboard_once(entry: &HistoryEntry) -> Result<()> {
    let mut clipboard = arboard::Clipboard::new().map_err(map_clipboard_error)?;

    if entry.mime_type == "text/plain" {
        let Some(text) = entry.text.as_ref() else {
            bail!("文本记录缺少内容");
        };
        clipboard
            .set_text(text.clone())
            .map_err(map_clipboard_error)?;
        return Ok(());
    }

    if entry.mime_type == "image/png" {
        let Some(encoded) = entry.image_base64.as_ref() else {
            bail!("图片记录缺少内容");
        };
        let bytes = BASE64.decode(encoded)?;
        let dynamic = image::load_from_memory_with_format(&bytes, ImageFormat::Png)?;
        let rgba = dynamic.to_rgba8();
        let data = arboard::ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: Cow::Owned(rgba.into_raw()),
        };
        clipboard.set_image(data).map_err(map_clipboard_error)?;
        return Ok(());
    }

    bail!("不支持的剪贴板类型：{}", entry.mime_type)
}

fn map_clipboard_error(error: arboard::Error) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

/// Starts the clipboard watcher and forwards one signal per detected change.
///
/// On Windows this uses `AddClipboardFormatListener` on a hidden message-only
/// window; other platforms fall back to polling so the code stays testable.
pub fn spawn_watcher(sender: UnboundedSender<()>) -> Result<()> {
    #[cfg(windows)]
    {
        match windows_listener::spawn(sender.clone()) {
            Ok(()) => return Ok(()),
            Err(error) => {
                eprintln!("[警告] 剪贴板事件监听不可用（{error}），改用 700ms 轮询。");
            }
        }
    }

    #[cfg(not(windows))]
    let _ = &sender;

    std::thread::Builder::new()
        .name("clipvault-clipboard-poll".to_string())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_millis(700));
            if sender.send(()).is_err() {
                break;
            }
        })?;

    Ok(())
}

#[cfg(windows)]
mod windows_listener {
    use std::sync::mpsc::{self, RecvTimeoutError};
    use std::sync::OnceLock;
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedSender;
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, RemoveClipboardFormatListener,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, RegisterClassW,
        TranslateMessage, HWND_MESSAGE, MSG, WM_CLIPBOARDUPDATE, WNDCLASSW,
    };

    static SENDER: OnceLock<UnboundedSender<()>> = OnceLock::new();

    pub fn spawn(sender: UnboundedSender<()>) -> Result<(), String> {
        SENDER
            .set(sender)
            .map_err(|_| "剪贴板监听已经启动".to_string())?;

        let (ready_sender, ready_receiver) = mpsc::channel::<bool>();
        std::thread::Builder::new()
            .name("clipvault-clipboard-events".to_string())
            .spawn(move || run_message_loop(ready_sender))
            .map_err(|error| error.to_string())?;

        match ready_receiver.recv_timeout(Duration::from_secs(3)) {
            Ok(true) => Ok(()),
            Ok(false) => Err("创建隐藏消息窗口失败".to_string()),
            Err(RecvTimeoutError::Timeout) => Err("等待剪贴板监听就绪超时".to_string()),
            Err(RecvTimeoutError::Disconnected) => Err("剪贴板监听线程提前退出".to_string()),
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if message == WM_CLIPBOARDUPDATE {
            if let Some(sender) = SENDER.get() {
                let _ = sender.send(());
            }
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
    }

    fn run_message_loop(ready: mpsc::Sender<bool>) {
        unsafe {
            let instance = GetModuleHandleW(std::ptr::null());
            if instance.is_null() {
                let _ = ready.send(false);
                return;
            }

            let class_name: Vec<u16> =
                format!("ClipVaultClipboardListener{}\0", std::process::id())
                    .encode_utf16()
                    .collect();

            let window_class = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(window_proc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: instance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class_name.as_ptr(),
            };

            if RegisterClassW(&window_class) == 0 {
                let _ = ready.send(false);
                return;
            }

            let hwnd = CreateWindowExW(
                0,
                class_name.as_ptr(),
                std::ptr::null(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                instance,
                std::ptr::null(),
            );
            if hwnd.is_null() {
                let _ = ready.send(false);
                return;
            }

            if AddClipboardFormatListener(hwnd) == 0 {
                let _ = ready.send(false);
                return;
            }

            let _ = ready.send(true);

            let mut message: MSG = std::mem::zeroed();
            loop {
                let result = GetMessageW(&mut message, std::ptr::null_mut(), 0, 0);
                if result == 0 || result == -1 {
                    break;
                }
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }

            RemoveClipboardFormatListener(hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "reads the real system clipboard; run manually with --ignored --nocapture"]
    fn manual_read_current_clipboard() {
        let device = DeviceInfo {
            id: "manual".into(),
            name: "manual".into(),
            platform: "windows".into(),
            role: "desktop".into(),
        };
        match read_clipboard(&device, false) {
            Ok(Some((signature, entry))) => {
                println!("signature={signature} preview={}", entry.preview)
            }
            Ok(None) => println!("clipboard has nothing to sync"),
            Err(error) => println!("error: {error:#}"),
        }
    }
}
