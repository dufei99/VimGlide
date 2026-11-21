//! 鼠标输入注入。

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
    MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

const WHEEL_DELTA: i32 = 120;

/// 把屏幕坐标换算成 SendInput 需要的 0..=65535 归一化坐标（覆盖整个虚拟桌面）。
fn to_absolute(x: i32, y: i32) -> (i32, i32) {
    // SAFETY: 纯查询调用。
    let (left, top, width, height) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let span_x = (width - 1).max(1) as i64;
    let span_y = (height - 1).max(1) as i64;
    let nx = ((x - left) as i64 * 65535 / span_x).clamp(0, 65535) as i32;
    let ny = ((y - top) as i64 * 65535 / span_y).clamp(0, 65535) as i32;
    (nx, ny)
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) {
    // SAFETY: 传入的 INPUT 数组布局与长度均由 SendInput 约定保证。
    unsafe {
        SendInput(inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

pub fn move_to(x: i32, y: i32) {
    let (nx, ny) = to_absolute(x, y);
    send(&[mouse_input(
        nx,
        ny,
        0,
        MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
    )]);
}

pub fn click(x: i32, y: i32) {
    move_to(x, y);
    sleep(Duration::from_millis(10));
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
    sleep(Duration::from_millis(10));
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
}

/// 双击（桌面图标、文件列表等需要双击才会打开）。
pub fn double_click(x: i32, y: i32) {
    move_to(x, y);
    sleep(Duration::from_millis(10));
    for _ in 0..2 {
        send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
        sleep(Duration::from_millis(10));
        send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
        sleep(Duration::from_millis(40));
    }
}

/// 滚轮：vertical 为垂直刻度（正=向下），horizontal 为水平刻度（正=向右）。
pub fn wheel(vertical: i32, horizontal: i32) {
    if vertical != 0 {
        send(&[mouse_input(
            0,
            0,
            vertical * WHEEL_DELTA,
            MOUSEEVENTF_WHEEL,
        )]);
    }
    if horizontal != 0 {
        send(&[mouse_input(
            0,
            0,
            horizontal * WHEEL_DELTA,
            MOUSEEVENTF_HWHEEL,
        )]);
    }
}
