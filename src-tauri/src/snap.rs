//! 统一截图工具（2026-11-01）：一个快捷键 → 冻结整屏 → 框选（可微调/吸附）→
//! 选区动作条（复制图片 / 提取文字 / 翻译 / 钉图）。
//!
//! 为什么是「先截整屏、再冻结框选」而不是系统原生 screencapture：
//! - 框选结束后要在选区旁显示动作条，系统 `screencapture -i` 的交互过程
//!   不归应用，无法插入自定义 UI；Windows 也没有等价的系统级框选。
//! - 冻结帧让框选、复制、识别、钉图都基于同一张图：不会出现「识别时再截一次」
//!   的过渡帧（快速手势截到遮罩淡出/动画未稳的上一版缺陷从根上消失），
//!   选区微调也不必重截。
//! - 吸附数据（窗口矩形 / 文字行矩形）都可以在这张冻结图上提前计算，坐标
//!   与框选 UI 天然一致。
//!
//! 流程：shortcut → capture_monitor（macOS screencapture -R / Windows helper
//! BitBlt）→ 建 snap 遮罩窗（整屏显示冻结图）→ TS 框选/微调/吸附 →
//! invoke snap_action（裁剪 + 分发）→ 关闭遮罩 → 动作在后台线程执行。
//!
//! 截图期间同样持 `CaptureMuteGuard`（AGENTS.md：模态鼠标操作须静音划词管线）。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;
use tauri::{AppHandle, Manager, WebviewUrl, WebviewWindow, WebviewWindowBuilder, WindowEvent};

use crate::capture::CaptureMuteGuard;

/// 选区/吸附矩形（逻辑坐标，相对遮罩窗 = 所在显示器左上角）
#[derive(Clone, Copy, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SnapRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// TS 挂载时拉取的会话信息
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapSessionInfo {
    /// 冻结整屏 PNG 路径（前端经 convertFileSrc 显示）
    pub image: String,
    /// 显示器逻辑尺寸（= 遮罩窗尺寸）
    pub width: f64,
    pub height: f64,
    /// 物理像素 / 逻辑点（选区尺寸标签按物理像素显示，与原图一致）
    pub scale: f64,
    /// 可吸附的窗口矩形（同一坐标系）
    pub windows: Vec<SnapRect>,
}

struct Session {
    /// 会话代号（与 SNAP_GEN 对应，后台任务/看门狗据此丢弃过期动作）
    generation: u64,
    /// 冻结整屏截图
    image: PathBuf,
    /// 截图物理像素 / 逻辑点（Retina 2、Windows 125% 等）
    scale: f64,
    logical: (f64, f64),
    /// 显示器物理原点（Windows 重截时用）
    origin: (i32, i32),
    /// 显示器逻辑原点（macOS screencapture -R / 钉图锚点用）
    logical_origin: (f64, f64),
    windows: Vec<SnapRect>,
}

static SESSION: Mutex<Option<Session>> = Mutex::new(None);
/// 文字行矩形（后台识别填充；为空 = 尚未就绪，吸附自动退化为窗口/屏幕边缘）
static TEXT_RECTS: Mutex<Vec<SnapRect>> = Mutex::new(Vec::new());
/// 遮罩窗存续期间的捕获静音守卫
static SNAP_MUTE: Mutex<Option<CaptureMuteGuard>> = Mutex::new(None);
/// 会话代号：后台文字矩形任务完成时据此丢弃过期结果（上一会话的活不能
/// 写进下一会话的吸附数据）
static SNAP_GEN: AtomicU64 = AtomicU64::new(0);
/// 会话闸门：从按下快捷键到遮罩关闭/动作完成，全程只允许一套截图会话；
/// 连按不会并发截图，也不会出现两个全屏遮罩
static SNAP_BUSY: AtomicBool = AtomicBool::new(false);
/// 前端是否已把遮罩内容准备好（收到 snap_ready 才真正显示窗口）。
/// 遮罩是全屏置顶层：页面没加载时也绝不显示，从根上避免「透明窗口吞点击」
static SNAP_READY: AtomicBool = AtomicBool::new(false);
/// 最近一次前端鼠标事件时间（UNIX 毫秒）：用于识别「窗口可见但输入完全死掉」
static SNAP_TOUCH_MS: AtomicU64 = AtomicU64::new(0);

/// 统一截图入口（全局快捷键 / 托盘菜单共用）。
/// 窗口创建必须在主线程；快捷键 handler 虽在事件线程，也统一经
/// run_on_main_thread 派发，避免平台差异（与 Windows 既有模式一致）。
pub fn start_capture_tool(app: &AppHandle) {
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if app2.get_webview_window("snap").is_some() {
            // 连按快捷键：旧遮罩尚未完成异步销毁，重复建同 label 窗口会 panic
            return;
        }
        if SNAP_BUSY.swap(true, Ordering::Relaxed) {
            return;
        }
        let mon = match cursor_monitor(&app2) {
            Ok(m) => m,
            Err(e) => {
                SNAP_BUSY.store(false, Ordering::Relaxed);
                crate::toast::show_toast(&app2, &e, "err");
                return;
            }
        };
        // 整屏截图（screencapture / helper BitBlt）是阻塞调用：不能占着
        // 快捷键 handler 的主线程（它还持有 global-shortcut 插件锁）
        let app3 = app2.clone();
        std::thread::spawn(move || match prepare_session(&mon) {
            Ok(()) => {
                let app4 = app3.clone();
                if app3
                    .run_on_main_thread(move || {
                        if let Err(e) = show_overlay(&app4) {
                            crate::user_log(&format!("[snap] 启动失败：{e}"));
                            crate::toast::show_toast(&app4, &e, "err");
                        }
                    })
                    .is_err()
                {
                    SNAP_BUSY.store(false, Ordering::Relaxed);
                    clear_session();
                }
            }
            Err(e) => {
                SNAP_BUSY.store(false, Ordering::Relaxed);
                crate::user_log(&format!("[snap] 启动失败：{e}"));
                crate::toast::show_toast(&app3, &e, "err");
            }
        });
    });
}

/// 后台线程：权限预检 + 冻结整屏 + 计算缩放/吸附数据。
/// 成功后才回到主线程建遮罩窗（窗口创建必须在主线程）。
fn prepare_session(mon: &tauri::Monitor) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    if !crate::ocr::screen_capture_preflight() {
        let _ = Command::new("open")
            .arg("x-apple.systempreferences:com.apple.preference.security?ScreenCapture")
            .spawn();
        return Err("需要屏幕录制权限\n已打开系统设置，授权后重试".into());
    }

    // 静音先于截图：整段截图模式期间都不允许划词管线响应鼠标
    *SNAP_MUTE.lock().unwrap() = Some(CaptureMuteGuard::new());
    let image = match capture_monitor(mon) {
        Ok(p) => p,
        Err(e) => {
            *SNAP_MUTE.lock().unwrap() = None;
            return Err(e);
        }
    };
    let (iw, ih) = match image::image_dimensions(&image) {
        Ok(v) => v,
        Err(e) => {
            let _ = std::fs::remove_file(&image);
            *SNAP_MUTE.lock().unwrap() = None;
            return Err(format!("截图读取失败: {e}"));
        }
    };
    if iw == 0 || ih == 0 {
        let _ = std::fs::remove_file(&image);
        *SNAP_MUTE.lock().unwrap() = None;
        return Err("截图尺寸无效".into());
    }
    let s = mon.scale_factor();
    let msize = mon.size();
    let logical = (msize.width as f64 / s, msize.height as f64 / s);
    if logical.0 < 1.0 || logical.1 < 1.0 {
        let _ = std::fs::remove_file(&image);
        *SNAP_MUTE.lock().unwrap() = None;
        return Err("无法读取屏幕尺寸".into());
    }
    // 用图片实际像素算缩放：多显示器混合 DPI 时比 scale_factor 更可靠
    let scale = iw as f64 / logical.0;
    let pos = mon.position();
    let logical_origin = (pos.x as f64 / s, pos.y as f64 / s);
    let windows = window_rects(&mon);

    let generation = SNAP_GEN.fetch_add(1, Ordering::Relaxed) + 1;
    SNAP_READY.store(false, Ordering::Relaxed);
    SNAP_TOUCH_MS.store(0, Ordering::Relaxed);
    *TEXT_RECTS.lock().unwrap() = Vec::new();
    *SESSION.lock().unwrap() = Some(Session {
        generation,
        image: image.clone(),
        scale,
        logical,
        origin: (pos.x, pos.y),
        logical_origin,
        windows,
    });

    spawn_text_rects_job(image, logical, generation);

    Ok(())
}

/// 主线程：建遮罩窗（尺寸/位置取自已冻结的会话）。
/// 窗口先以隐藏状态创建，等前端把冻结图渲染好、调用 snap_ready 后才显示——
/// 全屏置顶窗一旦可见就会吞掉所有鼠标事件，页面没画出来时绝不能让它在屏幕上。
fn show_overlay(app: &AppHandle) -> Result<(), String> {
    let (logical, logical_origin, generation) = {
        let guard = SESSION.lock().unwrap();
        let s = guard.as_ref().ok_or_else(|| "截图会话已结束".to_string())?;
        (s.logical, s.logical_origin, s.generation)
    };
    let builder = WebviewWindowBuilder::new(app, "snap", WebviewUrl::App("snap.html".into()))
        .title("拾趣截图")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .resizable(false)
        .visible(false)
        .position(logical_origin.0, logical_origin.1)
        .inner_size(logical.0, logical.1);
    // macOS：遮罩是 modal 交互，首次点击就要落到遮罩上（不等应用激活）
    #[cfg(target_os = "macos")]
    let builder = builder.accept_first_mouse(true);
    let win = builder.build();
    let win = match win {
        Ok(w) => w,
        Err(e) => {
            crate::user_log(&format!("[snap] 遮罩窗创建失败：{e}"));
            clear_session();
            return Err("无法打开截图窗口\n请重试".into());
        }
    };
    crate::user_log("[snap] 遮罩窗已创建（隐藏，等待前端就绪）");
    let app_watch = app.clone();
    win.on_window_event(|event| {
        if matches!(event, WindowEvent::Destroyed) {
            clear_session();
        }
    });
    std::thread::spawn(move || overlay_watchdog(app_watch, generation));
    Ok(())
}

/// 遮罩看门狗：保证任何前端/焦点异常都不会让全屏置顶层永久吞点击。
/// 1. 建窗后 8s 内没收到 snap_ready（页面没加载出来）→ 关闭并提示；
/// 2. 收到 ready 后 20s 内没有任何鼠标事件（窗口可见但输入链路死掉）→ 关闭并提示。
fn overlay_watchdog(app: AppHandle, generation: u64) {
    // 1) 等待前端就绪（100ms 粒度轮询，便于随时感知会话已取消）
    for _ in 0..80 {
        if SNAP_GEN.load(Ordering::Relaxed) != generation {
            return;
        }
        if SNAP_READY.load(Ordering::Relaxed) {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !SNAP_READY.load(Ordering::Relaxed) {
        crate::user_log("[snap] 前端 8s 未就绪，自动关闭遮罩");
        abort_overlay(&app, generation, "截图界面未能加载\n请重试");
        return;
    }

    // 2) 就绪后给一次真实的鼠标事件；完全收不到说明窗口没拿到输入焦点
    let ready_at = SNAP_TOUCH_MS.load(Ordering::Relaxed);
    std::thread::sleep(Duration::from_secs(20));
    if SNAP_GEN.load(Ordering::Relaxed) != generation {
        return;
    }
    if SNAP_TOUCH_MS.load(Ordering::Relaxed) == ready_at {
        crate::user_log("[snap] 遮罩 20s 未收到任何鼠标事件，判定输入失效，自动关闭");
        abort_overlay(&app, generation, "截图界面没有响应\n请重试");
    }
}

/// 关窗收尾（可带用户提示）；只在当前会话仍匹配时执行，避免误杀新会话
fn abort_overlay(app: &AppHandle, generation: u64, msg: &str) {
    if SNAP_GEN.load(Ordering::Relaxed) != generation {
        return;
    }
    let app2 = app.clone();
    let msg = msg.to_string();
    let _ = app.run_on_main_thread(move || {
        if SNAP_GEN.load(Ordering::Relaxed) != generation {
            return;
        }
        if let Some(w) = app2.get_webview_window("snap") {
            let _ = w.hide();
        }
        clear_session();
        if !msg.is_empty() {
            crate::toast::show_toast(&app2, &msg, "err");
        }
    });
}

/// macOS 原生层：截图遮罩必须压在菜单栏/Dock 之上，并且能出现在全屏应用的
/// Space 里——tao 的 always_on_top 只给 NSFloatingWindowLevel(3)，低于
/// 菜单栏(24)/Dock(20)，顶部会漏出一条实时界面、框选也框不进去。
#[cfg(target_os = "macos")]
fn configure_overlay_window(win: &tauri::WebviewWindow) {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;

    if let Ok(ns) = win.ns_window() {
        unsafe {
            let obj = ns as *mut AnyObject;
            // NSStatusWindowLevel = 25；CanJoinAllSpaces(1) | FullScreenAuxiliary(256)
            let _: () = msg_send![obj, setLevel: 25isize];
            let _: () = msg_send![obj, setCollectionBehavior: 257u64];
        }
    }
}

#[cfg(target_os = "macos")]
fn activate_app() {
    use objc2::msg_send;
    use objc2::runtime::AnyObject;
    // 显式截图模式允许抢焦点（与划词浮层的 orderFrontRegardless 语义不同）
    unsafe {
        let cls = objc2::class!(NSApplication);
        let app: *mut AnyObject = msg_send![cls, sharedApplication];
        if !app.is_null() {
            let _: () = msg_send![app, activateIgnoringOtherApps: true];
        }
    }
}

fn clear_session() {
    SNAP_BUSY.store(false, Ordering::Relaxed);
    let session = SESSION.lock().unwrap().take();
    release_ui_state();
    if let Some(s) = session {
        let _ = std::fs::remove_file(&s.image);
    }
}

/// 收尾时先隐藏窗口、释放状态，再排到主线程稍后销毁。
/// 不在取消命令里同步 close：避免「销毁正在执行 IPC 的 webview」这类重入问题；
/// 即使销毁环节异常，屏幕也已经先恢复可用。
fn destroy_overlay_soon(app: &AppHandle) {
    let app2 = app.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(120));
        let app3 = app2.clone();
        let _ = app2.run_on_main_thread(move || {
            if let Some(w) = app3.get_webview_window("snap") {
                let _ = w.close();
            }
        });
    });
}

/// 释放与遮罩窗绑定的运行时状态（不含冻结图：动作线程还要用它裁剪）
fn release_ui_state() {
    // 让仍在跑的文字矩形任务知道会话已结束（其结果不再写入）
    SNAP_GEN.fetch_add(1, Ordering::Relaxed);
    SNAP_READY.store(false, Ordering::Relaxed);
    SNAP_TOUCH_MS.store(0, Ordering::Relaxed);
    *SNAP_MUTE.lock().unwrap() = None;
    TEXT_RECTS.lock().unwrap().clear();
}

fn temp_png(prefix: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "{prefix}{}-{}.png",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    ))
}

// ---------------- 平台：光标所在显示器 / 整屏截图 / 窗口矩形 ----------------

fn cursor_monitor(app: &AppHandle) -> Result<tauri::Monitor, String> {
    // tauri 的 cursor_position / monitor_from_point 都是物理坐标，跨平台一致
    let monitor = app
        .cursor_position()
        .ok()
        .and_then(|p| app.monitor_from_point(p.x, p.y).ok().flatten());
    monitor
        .or_else(|| app.primary_monitor().ok().flatten())
        .ok_or_else(|| "无法定位光标所在显示器".into())
}

#[cfg(target_os = "macos")]
fn capture_monitor(mon: &tauri::Monitor) -> Result<PathBuf, String> {
    let s = mon.scale_factor();
    let pos = mon.position();
    let size = mon.size();
    // screencapture -R 的坐标系是屏幕点（逻辑坐标），不是物理像素
    let (x, y) = (pos.x as f64 / s, pos.y as f64 / s);
    let (w, h) = (size.width as f64 / s, size.height as f64 / s);
    let image = temp_png("magpie-snap-");
    let _ = std::fs::remove_file(&image);
    // -x 静音快门；-R 指定区域，不弹系统交互遮罩
    let status = Command::new("screencapture")
        .args(["-x", "-R", &format!("{x},{y},{w},{h}")])
        .arg(&image)
        .status()
        .map_err(|e| format!("无法启动截图: {e}"))?;
    if !(status.success() && image.exists()) {
        let _ = std::fs::remove_file(&image);
        return Err("截图失败".into());
    }
    Ok(image)
}

#[cfg(target_os = "windows")]
fn capture_monitor(mon: &tauri::Monitor) -> Result<PathBuf, String> {
    let pos = mon.position();
    let size = mon.size();
    let image = temp_png("magpie-snap-");
    let helper = crate::ocr::helper_path()?;
    let mut cmd = Command::new(&helper);
    cmd.arg("--snap")
        .arg(format!(
            "{},{},{},{}",
            pos.x, pos.y, size.width, size.height
        ))
        .arg(&image);
    hide_console(&mut cmd);
    let out = cmd.output().map_err(|e| format!("无法启动截屏进程: {e}"))?;
    if !out.status.success() || !image.exists() {
        let _ = std::fs::remove_file(&image);
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            "截图失败".into()
        } else {
            stderr.strip_prefix("ERR:").unwrap_or(&stderr).to_string()
        });
    }
    Ok(image)
}

#[cfg(target_os = "windows")]
fn hide_console(cmd: &mut Command) {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(0x0800_0000);
}

#[cfg(not(target_os = "windows"))]
fn hide_console(_cmd: &mut Command) {}

/// 可吸附的窗口矩形。v1 只取普通层窗口（layer 0），排除自身进程；
/// 返回值已换算到遮罩窗逻辑坐标。
#[cfg(target_os = "macos")]
fn window_rects(mon: &tauri::Monitor) -> Vec<SnapRect> {
    use std::ffi::{c_char, c_void};

    type CFArrayRef = *const c_void;
    type CFDictionaryRef = *const c_void;
    type CFStringRef = *const c_void;
    type CFNumberRef = *const c_void;

    #[link(name = "CoreGraphics", kind = "framework")]
    extern "C" {
        fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFArrayGetCount(arr: CFArrayRef) -> isize;
        fn CFArrayGetValueAtIndex(arr: CFArrayRef, idx: isize) -> *const c_void;
        fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
        fn CFNumberGetValue(num: CFNumberRef, the_type: i32, value_ptr: *mut c_void) -> u8;
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        // 与 capture/macos/ffi.rs 的声明保持一致（clashing_extern_declarations）
        fn CFRelease(cf: *mut c_void);
    }

    const K_CF_STRING_ENCODING_UTF8: u32 = 0x0800_0100;
    const K_CF_NUMBER_FLOAT64: i32 = 6;
    // kCGWindowListOptionOnScreenOnly = 1 << 0
    const K_CG_WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1;

    unsafe {
        let key = |s: &[u8]| {
            CFStringCreateWithCString(
                std::ptr::null(),
                s.as_ptr() as *const c_char,
                K_CF_STRING_ENCODING_UTF8,
            )
        };
        let k_bounds = key(b"kCGWindowBounds\0");
        let k_layer = key(b"kCGWindowLayer\0");
        let k_pid = key(b"kCGWindowOwnerPID\0");
        let k_x = key(b"X\0");
        let k_y = key(b"Y\0");
        let k_w = key(b"Width\0");
        let k_h = key(b"Height\0");

        let read_num = |dict: CFDictionaryRef, key: *const c_void| -> f64 {
            let v = CFDictionaryGetValue(dict, key);
            if v.is_null() {
                return 0.0;
            }
            let mut d = 0.0f64;
            let _ = CFNumberGetValue(
                v as CFNumberRef,
                K_CF_NUMBER_FLOAT64,
                &mut d as *mut _ as *mut c_void,
            );
            d
        };

        let arr = CGWindowListCopyWindowInfo(K_CG_WINDOW_LIST_ON_SCREEN_ONLY, 0);
        if arr.is_null() {
            for k in [k_bounds, k_layer, k_pid, k_x, k_y, k_w, k_h] {
                CFRelease(k as *mut c_void);
            }
            return Vec::new();
        }

        let s = mon.scale_factor();
        let pos = mon.position();
        let origin = (pos.x as f64 / s, pos.y as f64 / s);
        let (mw, mh) = (mon.size().width as f64 / s, mon.size().height as f64 / s);
        let own_pid = std::process::id();
        let n = CFArrayGetCount(arr);
        let mut out = Vec::new();
        for i in 0..n {
            let dict = CFArrayGetValueAtIndex(arr, i) as CFDictionaryRef;
            if dict.is_null() {
                continue;
            }
            let layer = read_num(dict, k_layer);
            if layer != 0.0 {
                continue;
            }
            if read_num(dict, k_pid) as u32 == own_pid {
                continue;
            }
            let bounds = CFDictionaryGetValue(dict, k_bounds) as CFDictionaryRef;
            if bounds.is_null() {
                continue;
            }
            let (x, y, w, h) = (
                read_num(bounds, k_x),
                read_num(bounds, k_y),
                read_num(bounds, k_w),
                read_num(bounds, k_h),
            );
            if w < 40.0 || h < 24.0 {
                continue;
            }
            let (rx, ry) = (x - origin.0, y - origin.1);
            if rx + w <= 0.0 || ry + h <= 0.0 || rx >= mw || ry >= mh {
                continue;
            }
            let nx = rx.max(0.0);
            let ny = ry.max(0.0);
            out.push(SnapRect {
                x: nx,
                y: ny,
                w: w.min(mw - nx),
                h: h.min(mh - ny),
            });
        }
        CFRelease(arr as *mut c_void);
        for k in [k_bounds, k_layer, k_pid, k_x, k_y, k_w, k_h] {
            CFRelease(k as *mut c_void);
        }
        out
    }
}

#[cfg(target_os = "windows")]
fn window_rects(mon: &tauri::Monitor) -> Vec<SnapRect> {
    use std::ffi::c_void;
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowLongW, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        GWL_EXSTYLE, WS_EX_TOOLWINDOW,
    };

    struct Ctx {
        out: Vec<(i32, i32, i32, i32)>,
        mon: (i32, i32, i32, i32),
        own_pid: u32,
    }

    unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let ctx = &mut *(lparam.0 as *mut Ctx);
        if !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == ctx.own_pid {
            return BOOL(1);
        }
        let ex = GetWindowLongW(hwnd, GWL_EXSTYLE) as u32;
        if (ex & WS_EX_TOOLWINDOW.0) != 0 {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        let got = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut c_void,
            std::mem::size_of::<RECT>() as u32,
        );
        if got.is_err() {
            let _ = GetWindowRect(hwnd, &mut rect);
        }
        let (mw, mh, mx, my) = ctx.mon;
        let (x, y, w, h) = (
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
        );
        if w < 40 || h < 24 {
            return BOOL(1);
        }
        if x + w <= mx || y + h <= my || x >= mx + mw || y >= my + mh {
            return BOOL(1);
        }
        ctx.out.push((x, y, w, h));
        BOOL(1)
    }

    let pos = mon.position();
    let size = mon.size();
    let mut ctx = Ctx {
        out: Vec::new(),
        mon: (size.width as i32, size.height as i32, pos.x, pos.y),
        own_pid: std::process::id(),
    };
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut ctx as *mut Ctx as isize));
    }
    let scale = mon.scale_factor();
    let (lw, lh) = (size.width as f64 / scale, size.height as f64 / scale);
    ctx.out
        .into_iter()
        .filter_map(|(x, y, w, h)| {
            let rx = (x - pos.x) as f64 / scale;
            let ry = (y - pos.y) as f64 / scale;
            let rw = w as f64 / scale;
            let rh = h as f64 / scale;
            if rw < 8.0 || rh < 8.0 {
                return None;
            }
            let nx = rx.max(0.0);
            let ny = ry.max(0.0);
            let nw = rw.min(lw - nx);
            let nh = rh.min(lh - ny);
            (nw > 8.0 && nh > 8.0).then_some(SnapRect {
                x: nx,
                y: ny,
                w: nw,
                h: nh,
            })
        })
        .collect()
}

// ---------------- 文字行矩形（后台计算，供文字块吸附） ----------------

fn spawn_text_rects_job(image: PathBuf, logical: (f64, f64), generation: u64) {
    std::thread::spawn(move || {
        let Ok(helper) = crate::ocr::helper_path() else {
            return;
        };
        let mut cmd = Command::new(&helper);
        cmd.arg("--blocks").arg(&image);
        hide_console(&mut cmd);
        let Ok(out) = cmd.output() else {
            return;
        };
        if !out.status.success() {
            return;
        }
        let mut rects = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let parts: Vec<f64> = line
                .split(',')
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect();
            if parts.len() != 4 {
                continue;
            }
            let (x, y, w, h) = (
                parts[0] * logical.0,
                parts[1] * logical.1,
                parts[2] * logical.0,
                parts[3] * logical.1,
            );
            if w < 8.0 || h < 6.0 {
                continue;
            }
            rects.push(SnapRect { x, y, w, h });
        }
        if SNAP_GEN.load(Ordering::Relaxed) != generation {
            return; // 会话已结束/已换新，丢弃过期结果
        }
        crate::capture::debug_log(format!("[snap] 文字吸附矩形 {} 条", rects.len()));
        *TEXT_RECTS.lock().unwrap() = rects;
    });
}

// ---------------- 命令：会话 / 选区动作 ----------------

#[tauri::command]
pub fn snap_session() -> Result<SnapSessionInfo, String> {
    let guard = SESSION.lock().unwrap();
    let s = guard.as_ref().ok_or_else(|| "截图会话已结束".to_string())?;
    Ok(SnapSessionInfo {
        image: s.image.to_string_lossy().into_owned(),
        width: s.logical.0,
        height: s.logical.1,
        scale: s.scale,
        windows: s.windows.clone(),
    })
}

#[tauri::command]
pub fn snap_text_rects() -> Vec<SnapRect> {
    TEXT_RECTS.lock().unwrap().clone()
}

/// 前端已渲染好冻结图：此时才显示遮罩并夺取焦点/键盘。
/// 命令本身在主线程执行，窗口调用安全；macOS 需要先激活进程再让窗口成为 key。
#[tauri::command]
pub fn snap_ready(window: WebviewWindow) -> Result<(), String> {
    if window.label() != "snap" {
        return Err("非截图窗口".into());
    }
    SNAP_READY.store(true, Ordering::Relaxed);
    SNAP_TOUCH_MS.store(now_ms(), Ordering::Relaxed);
    #[cfg(target_os = "macos")]
    {
        configure_overlay_window(&window);
        activate_app();
    }
    let _ = window.show();
    let _ = window.set_focus();
    crate::user_log("[snap] 前端已就绪，显示遮罩");
    Ok(())
}

/// 前端鼠标活动心跳（节流后调用）：看门狗据此判断窗口输入链路是否真的可用
#[tauri::command]
pub fn snap_touch(window: WebviewWindow) {
    if window.label() == "snap" {
        SNAP_TOUCH_MS.store(now_ms(), Ordering::Relaxed);
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[tauri::command]
pub fn snap_cancel(app: AppHandle) {
    crate::user_log("[snap] 取消截图");
    if let Some(w) = app.get_webview_window("snap") {
        let _ = w.hide();
    }
    clear_session();
    destroy_overlay_soon(&app);
}

/// 选区动作：裁剪冻结图 → 关闭遮罩 → 后台执行。
/// action = copy / extract / translate / pin
#[tauri::command]
pub fn snap_action(
    app: AppHandle,
    action: String,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<(), String> {
    if w < 4.0 || h < 4.0 {
        return Err("选区过小".into());
    }
    let Some(session) = SESSION.lock().unwrap().take() else {
        return Err("截图会话已结束".into());
    };
    // 先把遮罩收起来，再把解码/裁剪/识别全部丢到后台线程：5K 截图解码
    // 若留在 IPC 主线程，按钮会「按下没反应」零点几秒
    release_ui_state();
    SNAP_BUSY.store(false, Ordering::Relaxed);
    if let Some(win) = app.get_webview_window("snap") {
        let _ = win.hide();
    }
    destroy_overlay_soon(&app);

    let Session {
        image,
        scale,
        origin,
        logical_origin,
        ..
    } = session;
    let rect = SnapRect { x, y, w, h };
    let anchor = (logical_origin.0 + x, logical_origin.1 + y);
    let app2 = app.clone();
    std::thread::spawn(move || {
        let crop = match crop_region(&image, scale, x, y, w, h) {
            Ok(p) => p,
            Err(e) => {
                // 技术原因进日志，界面只留一句普通用户能懂的话
                crate::user_log(&format!("[snap] 裁剪失败：{e}"));
                let _ = std::fs::remove_file(&image);
                crate::toast::show_toast(&app2, "截图处理失败\n请重新截图", "err");
                return;
            }
        };
        let _ = std::fs::remove_file(&image);
        match action.as_str() {
            "copy" => copy_image(&app2, &crop),
            "pin" => pin_image(&app2, &crop, anchor),
            "extract" => {
                ocr_flow(&app2, crop, rect, origin, logical_origin, scale, None);
            }
            "translate" => {
                let service = default_translate_service(&app2);
                ocr_flow(
                    &app2,
                    crop,
                    rect,
                    origin,
                    logical_origin,
                    scale,
                    Some(("translate".to_string(), Some(service))),
                );
            }
            other => {
                eprintln!("[snap] 未知动作：{other}");
                let _ = std::fs::remove_file(&crop);
            }
        }
    });
    Ok(())
}

fn crop_region(
    image: &Path,
    scale: f64,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Result<PathBuf, String> {
    let img = image::open(image).map_err(|e| format!("截图解码失败: {e}"))?;
    let (iw, ih) = (img.width() as i64, img.height() as i64);
    let x0 = ((x * scale).round() as i64).clamp(0, iw - 1);
    let y0 = ((y * scale).round() as i64).clamp(0, ih - 1);
    let x1 = (((x + w) * scale).round() as i64).clamp(x0 + 1, iw);
    let y1 = (((y + h) * scale).round() as i64).clamp(y0 + 1, ih);
    let crop = img.crop_imm(x0 as u32, y0 as u32, (x1 - x0) as u32, (y1 - y0) as u32);
    let path = temp_png("magpie-crop-");
    crop.save_with_format(&path, image::ImageFormat::Png)
        .map_err(|e| format!("截图保存失败: {e}"))?;
    Ok(path)
}

fn copy_image(app: &AppHandle, path: &Path) {
    let result = (|| -> Result<(), String> {
        let bytes = std::fs::read(path).map_err(|e| format!("读取截图失败: {e}"))?;
        let img = image::load_from_memory(&bytes).map_err(|e| format!("解码截图失败: {e}"))?;
        let rgba = img.to_rgba8();
        let data = arboard::ImageData {
            width: rgba.width() as usize,
            height: rgba.height() as usize,
            bytes: std::borrow::Cow::Owned(rgba.into_raw()),
        };
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_image(data))
            .map_err(|e| format!("写入剪贴板失败: {e}"))
    })();
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(path);
            crate::toast::show_toast(app, "已复制图片", "info");
        }
        Err(e) => {
            let _ = std::fs::remove_file(path);
            crate::user_log(&format!("[snap] 复制图片失败：{e}"));
            crate::toast::show_toast(app, "复制图片失败", "err");
        }
    }
}

fn pin_image(app: &AppHandle, path: &Path, anchor: (f64, f64)) {
    match crate::pin::pin_from_image_file(app, path.to_path_buf(), anchor) {
        Ok(()) => {
            let _ = std::fs::remove_file(path);
        }
        Err(e) => {
            let _ = std::fs::remove_file(path);
            crate::user_log(&format!("[snap] 钉图失败：{e}"));
            crate::toast::show_toast(app, &e, "err");
        }
    }
}

fn default_translate_service(app: &AppHandle) -> String {
    let s = crate::settings::current(app);
    let d = s.translate.default;
    if s.translate.enabled.iter().any(|e| e == &d) {
        d
    } else {
        s.translate.enabled.first().cloned().unwrap_or_default()
    }
}

fn ocr_flow(
    app: &AppHandle,
    image: PathBuf,
    rect: SnapRect,
    origin: (i32, i32),
    logical_origin: (f64, f64),
    scale: f64,
    run: Option<(String, Option<String>)>,
) {
    crate::toast::show_toast_sticky(app, "正在识别…", "progress");
    match crate::ocr::recognize_file(&image) {
        Ok(text) if !text.trim().is_empty() => {
            crate::toast::hide_toast(app);
            crate::ocr::push_ocr_result(app, text, image, run);
        }
        Err(e) if e != "未识别到文字" => {
            crate::user_log(&format!("[ocr] 识别失败：{e}；截图 {}", image.display()));
            let _ = std::fs::remove_file(&image);
            crate::toast::hide_toast(app);
            crate::toast::show_toast(app, &e, "err");
        }
        _ => {
            // 首帧为空（Ok 空串或「未识别到文字」）：可能是内容正在变化/截到
            // 过渡态。冻结帧本身已消除上一版的遮罩淡出问题，这里仍保留
            //「延迟重截一次」作为兜底
            std::thread::sleep(Duration::from_millis(350));
            match retry_capture(rect, origin, logical_origin, scale) {
                Some((text, retry_image)) => {
                    crate::user_log(&format!(
                        "[ocr] 重截自愈成功：{}x{} → len={}",
                        rect.w,
                        rect.h,
                        text.len()
                    ));
                    let _ = std::fs::remove_file(&image);
                    crate::toast::hide_toast(app);
                    crate::ocr::push_ocr_result(app, text, retry_image, run);
                }
                None => {
                    let dims = image::image_dimensions(&image)
                        .map(|(w, h)| format!("{w}x{h}"))
                        .unwrap_or_else(|_| "?".into());
                    crate::user_log(&format!(
                        "[ocr] 识别失败：未识别到文字；截图 {dims} {}",
                        image.display()
                    ));
                    crate::toast::hide_toast(app);
                    crate::toast::show_toast(
                        app,
                        "未检测到文字\n重新框选，确认区域内有清晰的文字",
                        "err",
                    );
                }
            }
        }
    }
}

/// 重截一次（按选区实时截屏，不含遮罩），返回识别文本与清晰截图
#[cfg(target_os = "macos")]
fn retry_capture(
    rect: SnapRect,
    _origin: (i32, i32),
    logical_origin: (f64, f64),
    _scale: f64,
) -> Option<(String, PathBuf)> {
    if rect.w < 12.0 || rect.h < 12.0 {
        return None;
    }
    let path = temp_png("magpie-retry-");
    let _ = std::fs::remove_file(&path);
    let (x, y) = (logical_origin.0 + rect.x, logical_origin.1 + rect.y);
    let status = Command::new("screencapture")
        .args(["-x", "-R", &format!("{x},{y},{},{}", rect.w, rect.h)])
        .arg(&path)
        .status()
        .ok()?;
    if !(status.success() && path.exists()) {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    match crate::ocr::recognize_file(&path) {
        Ok(text) if !text.trim().is_empty() => Some((text, path)),
        _ => {
            let _ = std::fs::remove_file(&path);
            None
        }
    }
}

#[cfg(target_os = "windows")]
fn retry_capture(
    rect: SnapRect,
    origin: (i32, i32),
    _logical_origin: (f64, f64),
    scale: f64,
) -> Option<(String, PathBuf)> {
    if rect.w < 12.0 || rect.h < 12.0 {
        return None;
    }
    let path = temp_png("magpie-retry-");
    let helper = crate::ocr::helper_path().ok()?;
    let x = origin.0 + (rect.x * scale).round() as i32;
    let y = origin.1 + (rect.y * scale).round() as i32;
    let w = (rect.w * scale).round() as i32;
    let h = (rect.h * scale).round() as i32;
    let mut cmd = Command::new(&helper);
    cmd.arg("--snap-ocr")
        .arg(format!("{x},{y},{w},{h}"))
        .arg(&path);
    hide_console(&mut cmd);
    let out = cmd.output().ok()?;
    if !out.status.success() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if text.is_empty() {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    Some((text, path))
}
