//! 通过 UI Automation 采集前台窗口中的可点击元素。

use std::collections::HashSet;
use std::time::{Duration, Instant};

use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationTreeWalker,
    UIA_ButtonControlTypeId, UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId,
    UIA_DataItemControlTypeId, UIA_EditControlTypeId, UIA_HyperlinkControlTypeId,
    UIA_ListItemControlTypeId, UIA_MenuItemControlTypeId, UIA_RadioButtonControlTypeId,
    UIA_SliderControlTypeId, UIA_SplitButtonControlTypeId, UIA_TabItemControlTypeId,
    UIA_TreeItemControlTypeId,
};
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

use crate::model::{Rect, Target};

const MAX_TARGETS: usize = 800;
const MAX_VISITED: usize = 5000;

pub struct UiaSource {
    automation: IUIAutomation,
}

impl UiaSource {
    pub fn new() -> Result<Self, String> {
        // SAFETY: CoInitializeEx 已在消息循环所在线程调用过。
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER) }
                .map_err(|e| format!("初始化 UI Automation 失败：{e}"))?;
        Ok(Self { automation })
    }

    /// 遍历前台窗口的无障碍树，返回可点击元素。
    ///
    /// `budget` 用于限制总耗时，避免在复杂应用上卡住遮罩层。
    pub fn collect(&self, screen: Rect, budget: Duration) -> Vec<Target> {
        let deadline = Instant::now() + budget;
        let mut targets = Vec::new();
        let mut seen: HashSet<(i32, i32, i32, i32)> = HashSet::new();

        // SAFETY: 本方法在拥有 COM 初始化的线程内调用，UIA 对象只在本次调用中使用。
        unsafe {
            let hwnd = GetForegroundWindow();
            let root = if hwnd.0.is_null() {
                self.automation.GetRootElement()
            } else {
                self.automation.ElementFromHandle(hwnd)
            };
            let root = match root {
                Ok(root) => root,
                Err(_) => return targets,
            };
            let walker: IUIAutomationTreeWalker = match self.automation.ControlViewWalker() {
                Ok(walker) => walker,
                Err(_) => return targets,
            };

            let mut stack = vec![root];
            let mut visited = 0usize;

            while let Some(parent) = stack.pop() {
                if Instant::now() >= deadline
                    || visited >= MAX_VISITED
                    || targets.len() >= MAX_TARGETS
                {
                    break;
                }

                let mut next = walker.GetFirstChildElement(&parent);
                loop {
                    let element = match next {
                        Ok(element) => element,
                        Err(_) => break,
                    };
                    visited += 1;

                    if let Some(target) = target_of(&element, screen) {
                        let key = (
                            target.rect.x,
                            target.rect.y,
                            target.rect.w,
                            target.rect.h,
                        );
                        if seen.insert(key) {
                            targets.push(target);
                        }
                    }

                    stack.push(element.clone());
                    next = walker.GetNextSiblingElement(&element);
                }
            }
        }

        // 按视觉顺序排列，提示标签读起来更顺手。
        targets.sort_by_key(|t| (t.rect.y, t.rect.x));
        targets
    }
}

/// 判断单个元素是否值得给一个提示标签。
fn target_of(element: &IUIAutomationElement, screen: Rect) -> Option<Target> {
    // SAFETY: element 来自当前 UIA 树，调用线程已初始化 COM。
    unsafe {
        let control_type = element.CurrentControlType().ok()?.0;
        if !is_clickable(control_type) {
            return None;
        }
        if !element.CurrentIsEnabled().ok()?.as_bool() {
            return None;
        }
        if element.CurrentIsOffscreen().ok()?.as_bool() {
            return None;
        }

        let bounds = element.CurrentBoundingRectangle().ok()?;
        let rect = Rect::new(
            bounds.left,
            bounds.top,
            bounds.right - bounds.left,
            bounds.bottom - bounds.top,
        );
        let visible = rect.intersection(&screen)?;
        if visible.w < 8 || visible.h < 8 {
            return None;
        }
        // 大部分在屏幕外的元素不参与提示，避免点到奇怪的位置。
        if visible.area() * 2 < rect.area() {
            return None;
        }

        let name = element
            .CurrentName()
            .map(|name| name.to_string())
            .unwrap_or_default();

        Some(Target { rect: visible, name })
    }
}

fn is_clickable(control_type: i32) -> bool {
    [
        UIA_ButtonControlTypeId.0,
        UIA_HyperlinkControlTypeId.0,
        UIA_MenuItemControlTypeId.0,
        UIA_TabItemControlTypeId.0,
        UIA_CheckBoxControlTypeId.0,
        UIA_RadioButtonControlTypeId.0,
        UIA_EditControlTypeId.0,
        UIA_ComboBoxControlTypeId.0,
        UIA_ListItemControlTypeId.0,
        UIA_DataItemControlTypeId.0,
        UIA_TreeItemControlTypeId.0,
        UIA_SliderControlTypeId.0,
        UIA_SplitButtonControlTypeId.0,
    ]
    .contains(&control_type)
}
