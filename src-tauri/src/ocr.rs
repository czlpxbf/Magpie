//! OCR 文本识别（父进程侧）：把截屏裁剪后的图片交给独立的 `ocr_helper`
//! 子进程执行（src/bin/ocr_helper.rs）——AppKit/Vision 的 ObjC 异常或崩溃
//! 只影响子进程，主应用任何情况下不会因此 abort。
//!
//! 交互式框选已迁至 snap.rs（统一截图工具：冻结整屏 + 自绘遮罩 + 选区动作条）。
//! 本模块只保留：助手进程识别、结果推送、模型预热与屏幕权限预检。

use std::path::{Path, PathBuf};
use std::process::Command;

use tauri::Emitter;

/// 用助手子进程识别图片文字（Vision；子进程隔离 ObjC 异常）。
/// 助手直连模式：`ocr_helper <图片路径>`，文本走 stdout。
pub fn recognize_file(path: &Path) -> Result<String, String> {
    let helper = helper_path()?;
    eprintln!("[ocr] 启动助手进程：{}", helper.display());
    let output = Command::new(&helper)
        .arg(path)
        .output()
        .map_err(|e| format!("无法启动 OCR 助手: {e}"))?;
    eprintln!("[ocr] 助手退出：status={}", output.status);

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        // 助手以 "ERR:<原因>" 报告失败原因，去掉前缀直接作为提示展示
        //（如「未识别到文字」，不需要「文本识别失败: ERR:」包装）
        if let Some(reason) = stderr.strip_prefix("ERR:") {
            return Err(reason.to_string());
        }
        return Err(format!(
            "文本识别失败{}",
            if stderr.is_empty() {
                String::new()
            } else {
                format!(": {stderr}")
            }
        ));
    }

    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return Err("未识别到文字".into());
    }
    eprintln!("[ocr] 识别成功：len={}", text.len());
    Ok(text)
}

/// 识别完成，一次性推送结果（文本 + 原图 + 可选自动执行动作）：面板出现即有
/// 完整内容，原图随 pending 保留供「钉图」。run = (动作 id, 服务)，
/// 传 (id, None) 时由前端按该动作既有的默认服务逻辑选择。
/// 事件用全局广播而不是 emit_to：JS listen() 注册的是 Any 目标，
/// emit_to 的按窗口过滤不匹配 Any 监听器（此前识别面板因此永不出现）。
/// 其他窗口收到后按 app=="ocr" 忽略。
pub fn push_ocr_result(
    app: &tauri::AppHandle,
    text: String,
    image: PathBuf,
    run: Option<(String, Option<String>)>,
) {
    super::floating::ocr_set_pending(text, Some(image), run);
    let _ = app.emit(
        "selection://captured",
        serde_json::json!({ "text": "", "x": -1.0, "y": -1.0, "app": "ocr" }),
    );
}

/// macOS 26+ 兼容：中文识别模型改为按需资源——系统更新后首次识别中文会
/// 阻塞十余秒等待模型拉取（实测 13.9s，CPU 占用极低 = 纯等待网络），
/// 之后系统级缓存，所有进程都在半秒内；旧版 macOS 模型内置，无此代价。
/// 启动后后台用内置的含中文小图跑一次微型 OCR 预热，把这次代价挪到
/// 非交互时刻——新旧版本用户在交互流程里都只体验半秒级识别。
pub fn warmup() {
    std::thread::spawn(move || {
        // 错开启动争抢窗口；纯后台，任何失败静默（预热失败 ≠ 功能失败，
        // 真正的识别有自己的错误提示）
        std::thread::sleep(std::time::Duration::from_secs(3));
        let Ok(exe) = std::env::current_exe() else {
            return;
        };
        let Some(dir) = exe.parent() else {
            return;
        };
        let helper = dir.join("ocr_helper");
        if !helper.exists() {
            return; // dev 缺助手二进制时静默跳过（cargo build --bin ocr_helper 可补）
        }
        let img = std::env::temp_dir().join("magpie-ocr-warmup.png");
        if std::fs::write(&img, include_bytes!("../icons/ocr-warmup.png")).is_err() {
            return;
        }
        let mut cmd = Command::new(&helper);
        cmd.arg(&img);
        #[cfg(target_os = "windows")]
        {
            // 隐藏控制台子进程的宿主黑窗
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        let _ = cmd.output();
        let _ = std::fs::remove_file(&img);
        eprintln!("[ocr] 中文模型预热完成");
    });
}

/// 把识别文本推给 OCR 独立窗口显示（钉图→识别文字流程：文本就绪、无图）。
/// 事件用全局广播而不是 emit_to：JS listen() 注册的是 Any 目标，
/// emit_to 的按窗口过滤不匹配 Any 监听器（此前识别面板因此永不出现）。
/// 其他窗口收到后按 app=="ocr" 忽略。
pub fn push_text_to_ocr_window(app: &tauri::AppHandle, text: String, image: Option<PathBuf>) {
    super::floating::ocr_set_pending(text, image, None);
    let _ = app.emit(
        "selection://captured",
        serde_json::json!({ "text": "", "x": -1.0, "y": -1.0, "app": "ocr" }),
    );
}

pub(crate) fn helper_path() -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("无法定位应用目录: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "无法定位应用目录".to_string())?;
    // Windows 上 std 不会为带目录的路径自动补 .exe，显式声明
    #[cfg(target_os = "windows")]
    return Ok(dir.join("ocr_helper.exe"));
    #[cfg(not(target_os = "windows"))]
    Ok(dir.join("ocr_helper"))
}

/// 屏幕录制权限预检（CoreGraphics；与 macos.rs 各自独立声明）
#[cfg(target_os = "macos")]
pub fn screen_capture_preflight() -> bool {
    extern "C" {
        fn CGPreflightScreenCaptureAccess() -> u8;
    }
    unsafe { CGPreflightScreenCaptureAccess() != 0 }
}
