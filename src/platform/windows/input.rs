//! 鼠标输入注入。

use std::thread::sleep;
use std::time::Duration;

use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_MOUSE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL,
    MOUSEINPUT, SendInput,
};
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

const WHEEL_DELTA: i32 = 120;

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
    // SAFETY: 纯定位调用，坐标超出桌面时系统会自动收敛到边缘。
    unsafe {
        let _ = SetCursorPos(x, y);
    }
}

/// 相对移动鼠标（像素）。
pub fn nudge(dx: i32, dy: i32) {
    let (x, y) = cursor_position();
    move_to(x + dx, y + dy);
}

/// 当前光标位置（物理像素）。
pub fn cursor_position() -> (i32, i32) {
    let mut point = POINT::default();
    // SAFETY: 纯查询调用，写入本栈上的 POINT。
    unsafe {
        let _ = GetCursorPos(&mut point);
    }
    (point.x, point.y)
}

/// 在当前位置单击，不移动鼠标（定位已由提示模式完成）。
pub fn click_here() {
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTDOWN)]);
    sleep(Duration::from_millis(10));
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_LEFTUP)]);
}

/// 在当前位置双击。
pub fn double_click_here() {
    for _ in 0..2 {
        click_here();
        sleep(Duration::from_millis(40));
    }
}

/// 在当前位置右击。
pub fn right_click_here() {
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_RIGHTDOWN)]);
    sleep(Duration::from_millis(10));
    send(&[mouse_input(0, 0, 0, MOUSEEVENTF_RIGHTUP)]);
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
