//! Window launcher: native WebView2 window on Windows, browser fallback everywhere else.
use std::sync::atomic::Ordering;

pub const TITLE: &str = "Job Finder";

pub fn message_box(text: &str, error: bool) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_ICONINFORMATION};
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        unsafe { MessageBoxW(std::ptr::null_mut(), w(text).as_ptr(), w(TITLE).as_ptr(), if error { MB_ICONERROR } else { MB_ICONINFORMATION }) };
        return;
    }
    #[allow(unreachable_code)]
    {
        eprintln!("{}: {}", TITLE, text);
    }
}

/// False when another copy of the program is already running.
pub fn acquire_single_instance() -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;
        let name: Vec<u16> = "Local\\JobFinder.SingleInstance".encode_utf16().chain(Some(0)).collect();
        let h = unsafe { CreateMutexW(std::ptr::null(), 0, name.as_ptr()) };
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            return false;
        }
        std::mem::forget(h); // lives as long as the process
        true
    }
    #[cfg(not(windows))]
    {
        match std::net::TcpListener::bind(("127.0.0.1", 47653)) {
            Ok(l) => {
                std::mem::forget(l);
                true
            }
            Err(_) => false,
        }
    }
}

#[cfg(windows)]
fn find_app_browser() -> Option<std::path::PathBuf> {
    let pf = |v: &str| std::env::var_os(v).map(std::path::PathBuf::from);
    let mut c = vec![];
    for v in ["ProgramFiles(x86)", "ProgramFiles", "LOCALAPPDATA"] {
        if let Some(p) = pf(v) {
            c.push(p.join(r"Microsoft\Edge\Application\msedge.exe"));
            c.push(p.join(r"Google\Chrome\Application\chrome.exe"));
        }
    }
    c.into_iter().find(|p| p.is_file())
}

pub fn open_fallback_ui(url: &str) {
    #[cfg(windows)]
    if let Some(exe) = find_app_browser() {
        if std::process::Command::new(exe).arg(format!("--app={}", url)).arg("--window-size=1240,900").spawn().is_ok() {
            return;
        }
    }
    let _ = open::that_detached(url);
}

/// Where the window was last time (size, position, maximised), kept in window.json next to the settings.
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct WinState {
    w: f64,
    h: f64,
    pos: Option<(i32, i32)>,
    maximized: bool,
}

#[cfg(windows)]
fn load_win_state() -> WinState {
    let mut st = WinState { w: 1240.0, h: 900.0, pos: None, maximized: false };
    if let Some(v) = std::fs::read(crate::util::base().join("window.json")).ok().and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok()) {
        let f = |k: &str| v.get(k).and_then(|x| x.as_f64());
        if let (Some(w), Some(h)) = (f("w"), f("h")) {
            st.w = w.clamp(960.0, 8000.0);
            st.h = h.clamp(640.0, 8000.0);
        }
        if let (Some(x), Some(y)) = (f("x"), f("y")) {
            st.pos = Some((x as i32, y as i32));
        }
        st.maximized = v.get("max").and_then(|x| x.as_bool()).unwrap_or(false);
    }
    st
}

#[cfg(windows)]
fn save_win_state(st: &WinState) {
    let mut v = serde_json::json!({"w": st.w.round(), "h": st.h.round(), "max": st.maximized});
    if let Some((x, y)) = st.pos {
        v["x"] = serde_json::json!(x);
        v["y"] = serde_json::json!(y);
    }
    let _ = crate::util::write_atomic(&crate::util::base().join("window.json"), |p| std::fs::write(p, v.to_string()));
}

/// Blocks until the window is closed. Err when no native window can be created.
#[cfg(windows)]
pub fn show_window(url: &str) -> Result<(), String> {
    use tao::dpi::{LogicalSize, PhysicalPosition};
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::platform::run_return::EventLoopExtRunReturn;
    use tao::window::WindowBuilder;
    use wry::{WebContext, WebViewBuilder};

    let mut event_loop = EventLoopBuilder::new().build();
    let mut st = load_win_state();
    // only reuse the old position when it is still on a connected screen (a monitor may have been unplugged)
    let on_screen = |x: i32, y: i32| {
        event_loop.available_monitors().any(|m| {
            let (p, sz) = (m.position(), m.size());
            x >= p.x - 40 && x <= p.x + sz.width as i32 - 120 && y >= p.y - 10 && y <= p.y + sz.height as i32 - 120
        })
    };
    if let Some((x, y)) = st.pos {
        if !on_screen(x, y) {
            st.pos = None;
        }
    }
    let mut wb = WindowBuilder::new().with_title(TITLE).with_inner_size(LogicalSize::new(st.w, st.h)).with_min_inner_size(LogicalSize::new(960.0, 640.0));
    if let Some((x, y)) = st.pos {
        wb = wb.with_position(PhysicalPosition::new(x, y));
    }
    if st.maximized {
        wb = wb.with_maximized(true);
    }
    let window = wb.build(&event_loop).map_err(|e| e.to_string())?;
    let mut ctx = WebContext::new(Some(crate::util::base().join("webview")));
    let _webview = WebViewBuilder::new_with_web_context(&mut ctx).with_url(url).build(&window).map_err(|e| e.to_string())?;
    let mut last_save = std::time::Instant::now();
    let mut dirty = false;
    event_loop.run_return(|event, _, flow| {
        *flow = ControlFlow::WaitUntil(std::time::Instant::now() + std::time::Duration::from_millis(250));
        let note = |window: &tao::window::Window, st: &mut WinState| {
            st.maximized = window.is_maximized();
            if !st.maximized && !window.is_minimized() {
                // the normal (restored) size and place are what we want back next time
                let l = window.inner_size().to_logical::<f64>(window.scale_factor());
                if l.width >= 300.0 && l.height >= 300.0 {
                    st.w = l.width.clamp(960.0, 8000.0);
                    st.h = l.height.clamp(640.0, 8000.0);
                }
                if let Ok(p) = window.outer_position() {
                    if p.x > -10000 && p.y > -10000 {
                        st.pos = Some((p.x, p.y));
                    }
                }
            }
        };
        match &event {
            Event::WindowEvent { event: WindowEvent::Resized(_), .. } | Event::WindowEvent { event: WindowEvent::Moved(_), .. } => {
                note(&window, &mut st);
                dirty = true;
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                note(&window, &mut st);
                save_win_state(&st);
                dirty = false;
                *flow = ControlFlow::Exit;
            }
            _ => {}
        }
        if dirty && last_save.elapsed() > std::time::Duration::from_secs(2) {
            save_win_state(&st); // also survives a crash or a kill from Task Manager
            last_save = std::time::Instant::now();
            dirty = false;
        }
        if crate::server::QUIT.load(Ordering::Relaxed) {
            if dirty {
                save_win_state(&st);
            }
            *flow = ControlFlow::Exit;
        }
    });
    Ok(())
}

#[cfg(not(windows))]
pub fn show_window(_url: &str) -> Result<(), String> {
    Err("native window is only available on Windows".into())
}

/// Windows: put this process in a job that kills every child process when the program ends - even when it is
/// killed from Task Manager or crashes - so no browser is ever left running.
pub fn kill_children_with_me() {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::System::JobObjects::*;
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if SetInformationJobObject(job, JobObjectExtendedLimitInformation, &info as *const _ as *const std::ffi::c_void, std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) != 0 {
            AssignProcessToJobObject(job, GetCurrentProcess());
        }
        // the handle stays open for the life of the process; closing it (process end) kills the job
    }
}
