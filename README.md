# VimGlide

系统级的 Vim 风格提示导航：在**任意** Windows 程序里按一下热键，当前前台窗口所有可点击元素都会浮出短标签，输入标签后鼠标自动移到目标位置，再按空格完成点击，全程不用鼠标。

```
┌─ 浏览器 / 资源管理器 / 设置 ┄┄┄┄┄┄┄┄┄┄┄┄┐
│  [sa] 新建标签页      [dk] 刷新          │
│  [fj] 地址栏          [lc] 收藏          │
│         VimGlide   42 targets   type label  …  │
└──────────────────────────────────────────┘
```

## 特性

- **全局可用**：基于 UI Automation 遍历前台窗口的无障碍树，浏览器、Electron、Win32/WinUI 应用都能识别。
- **助记式标签**：优先用元素名称首字母做单字符助记标签（如 `Save` → `s`），冲突时退化为双字符标签，不会出现「一个标签是另一个标签前缀」的歧义。
- **输入即筛选**：已输入前缀高亮，未匹配的标签自动隐藏，HUD 实时显示剩余数量。
- **两段式动作**：输入标签即高亮元素并把鼠标移到目标，`Space` 单击、`Shift+Space` 双击、`Ctrl+Space` 右击；选中后可用 `h` `j` `k` `l` 微调位置（默认每次 2 像素，`Shift` 10 像素，长按加速，均可在 `[move]` 中调整），误输入时 `Esc` 取消。
- **连续操作**：点击后若弹出菜单、对话框或新窗口，会自动为新出现的 UI 打标签，可继续输入标签操作，无需再次按热键；没有新 UI 时自动退出提示模式。
- **Vim 风格滚动**：提示模式下可直接用 `j k h l d u g G`、方向键、PageUp/PageDown、Home/End 滚动页面。
- **托盘常驻**：左键打开帮助，右键打开菜单（显示提示、打开配置、重新载入配置、帮助、以管理员身份重启、退出）。
- **热键被占用时自动退让**：默认热键注册失败会依次尝试 `ctrl+alt+space`、`alt+shift+space`。
- **完全可配置**：热键、标签字符表、配色、字号、滚动步长都写在 TOML 里。

## 平台支持

| 平台 | 状态 |
| --- | --- |
| Windows 10 / 11 | 完整实现 |
| Linux | 占位（仅有接口说明） |
| macOS | 占位（仅有接口说明） |

## 构建与运行

需要 Rust 1.85+（`edition = "2024"`）与 MSVC 工具链。

```powershell
cargo build --release
.\target\release\vimglide.exe
```

也可以直接 `cargo run --release`。程序启动后常驻托盘，不会弹出窗口。

带自定义配置启动：

```powershell
.\target\release\vimglide.exe --config D:\my\vimglide.toml
```

> 程序是单实例的，重复启动不会创建第二个进程。

## 使用

按 **`Ctrl+Shift+Space`** 唤出提示，然后：

| 按键 | 作用 |
| --- | --- |
| 标签字符 | 逐位输入，唯一确定后选中该元素并把鼠标移到点击位置（不立即点击） |
| `Space` | 对选中的元素执行单击（点击当前光标位置） |
| `Shift`+`Space` | 对选中的元素执行双击 |
| `Ctrl`+`Space` | 对选中的元素执行右击（弹出右键菜单会自动接续打标签） |
| `h` `j` `k` `l` | 选中后微调光标：默认每次 2 像素，`Shift` 10 像素，长按加速（见 `[move]`） |
| `?` | 重新显示帮助提示（标签出现后帮助会在 3 秒后自动消失） |
| `Backspace` | 回退一位；选中状态下则取消选中 |
| `Esc` | 取消选中；没有选中时退出提示模式 |
| `↑` `↓` `←` `→` | 按 `scroll.step_notches` 滚动 |
| `PageUp` / `PageDown` | 按 `scroll.page_notches` 滚动 |
| `Home` / `End` | 滚动到页首 / 页尾 |
| `j` `k` `h` `l` | 未选中时：下 / 上 / 左 / 右滚动（缓冲区为空且没有标签以该字母开头时生效） |
| `d` `u` | 半页下 / 上 |
| `g` `G` | 页首 / 页尾 |
| `Ctrl+Shift+Alt+Q` | 退出 VimGlide |

提示模式下键盘会被 VimGlide 暂时接管，`Esc` 或执行动作后立即恢复。

### 权限说明

UI Automation 无法读取权限层级更高的窗口。如果目标窗口以管理员身份运行，而 VimGlide 不是，HUD 会提示「前台窗口以管理员权限运行」；此时从托盘菜单选择「以管理员身份重启」即可。反向情况（VimGlide 是管理员、目标不是）可以正常识别。

### VS Code / Electron 类应用（如 Trae CN）

这类应用（Trae CN、VS Code、Cursor 等）基于 Chromium，**默认不构建无障碍树**，VimGlide 只能识别到窗口标题栏按钮（最小化/最大化/关闭），页面内容采不到。

解决办法是启动时加上 `--force-renderer-accessibility` 参数：

- **Trae CN**：右键快捷方式 → 属性 → 在「目标」末尾追加该参数；或命令行启动
  ```powershell
  & "$env:LOCALAPPDATA\Programs\Trae CN\Trae CN.exe" --force-renderer-accessibility
  ```
  （实际路径以安装位置为准。）
- **VS Code**：`code --force-renderer-accessibility`
- **其它 Electron 应用**：同样在启动参数里追加即可

无障碍树需要几秒钟构建，应用刚启动时标签可能很少，稍等片刻再按一次热键重新扫描即可。

## 配置

首次运行会自动生成 `%APPDATA%\VimGlide\config.toml`（带注释化的默认值）。修改后从托盘菜单选择**重新载入配置**即可生效（热键也会重新注册），无需重启。

```toml
[hotkey]
hint = "ctrl+shift+space"      # 唤出/退出提示模式
quit = "ctrl+shift+alt+q"      # 退出进程

[hint]
chars = "sadfjklewcmpgh"       # 标签字符表（自动去重，仅保留 a-z 0-9）
font_size = 14.0               # 逻辑像素，会按 DPI 缩放
label_bg = "#fdf6b266"         # 支持 #RRGGBB 或 #RRGGBBAA，末两位为透明度
label_fg = "#005700"
label_border = "#e2c94f"
matched_bg = "#7ee78799"       # 已匹配标签
matched_fg = "#0b2b13"
prefix_fg = "#b32020"          # 已输入前缀
outline = "#9a9a9a"            # 元素描边
outline_alpha = 90             # 0-255

[hud]
font_size = 14.0
bg = "#1f2430b3"               # 支持 #RRGGBBAA，末两位为透明度
fg = "#e6e6e6"
accent = "#7ee787"
flash = "#ff7b72"

[scroll]
step_notches = 3               # 方向键 / jklh 每次滚动的滚轮刻度
page_notches = 15              # PageUp / PageDown 每次滚动

[move]
step = 2                       # 选中后 hjkl 每次移动的像素
shift_step = 10                # Shift + hjkl 每次移动的像素
accel_ms = 150                 # 长按自动重复时，每过多少毫秒提速一档
accel_max = 16                 # 加速的最高倍率

[ocr]
enabled = true                 # UIA 采不到元素时用系统 OCR 兜底
min_uia_targets = 5            # UIA 目标少于该值时才启用 OCR
max_targets = 200              # OCR 追加目标上限，也是目标总数上限
```

热键写法为 `修饰键+修饰键+主键`，修饰键支持 `ctrl` / `shift` / `alt`（`option`）/ `win`（`meta`、`super`、`cmd`），主键支持 `a-z`、`0-9`、`f1`-`f24`、`space`、`esc`、`tab`、`enter`、方向键、`home`、`end`、`pageup`、`pagedown`、`insert`、`delete` 以及 `;` `/` `` ` `` `[` `\` `]` `'` `,` `.` `-` `=`。至少需要一个修饰键。

## 实现概览

```
src/
├── main.rs            入口：加载配置后进入平台消息循环
├── app.rs             与平台无关的状态机（提示 / 筛选 / 滚动 / 绘制）
├── hint.rs            标签生成与输入匹配（含单元测试）
├── config.rs          TOML 配置读写与默认值
├── model.rs           Rect / Target 等基础类型
├── render.rs          绘制指令（Scene / Op / 文字）与托盘图标用的软件画布
└── platform/
    ├── mod.rs         Backend trait 与 Key 抽象
    ├── windows/       全局热键、键盘钩子、UIA 采集、分层窗口遮罩、鼠标注入、托盘
    ├── linux/         占位
    └── macos/         占位
```

- 热键用 `RegisterHotKey` 注册；提示模式下的按键由 `WH_KEYBOARD_LL` 低级键盘钩子捕获后投递回消息循环（钩子内不做耗时操作，避免被系统跳过）。
- 元素来自 UI Automation 的 Control View 遍历，只收集按钮、链接、菜单项、列表项、选项卡等可点击类型，带 1.5 秒总预算与 800 个元素上限。
- 遮罩使用 `WS_EX_NOREDIRECTIONBITMAP` 窗口，内容由 DirectComposition 承载的 DXGI 交换链 + Direct2D 直接绘制，不分配整屏 CPU 位图；同时支持多显示器。
- 选中后用 `SetCursorPos` 把光标移到目标位置，`hjkl` 逐像素微调；点击通过 `SendInput` 在当前光标处注入真实鼠标事件，与用户手动点击等价。

## 已知限制

- 只识别支持 UI Automation 的控件；自绘界面或缺少无障碍信息的程序可能采不到元素。Chromium/Electron 类应用（如 Trae CN）默认不生成无障碍树，需要加 `--force-renderer-accessibility` 启动，详见[上文](#vs-code--electron-类应用如-trae-cn)。
- 提示模式会拦截所有键盘输入（包括输入法），`Esc` 可随时退出。
- 点击后的自动接续覆盖标准 Win32 菜单（类名 `#32768`）、对话框、新前台窗口，以及目标进程新出现的窗口化弹层（Chromium 下拉菜单、XAML 菜单等）；这类弹层 UIA 采不到时只对该弹层做一次 OCR（开销很小）。工具提示和页面内原地展开的内容不会触发接续。
- GTK 类程序（Inkscape、GIMP 等）在 Windows 上无法识别：GTK 只暴露一个 `gdkWindowToplevel` 顶层窗口，既不实现 UI Automation 也没有 MSAA 桥接，实测其无障碍子树为空，因此没有可标注的元素；GTK 在 Windows 上没有类似 `--force-renderer-accessibility` 的开关，属于上游限制。
- Windows 平台独占，尚未实现 Linux / macOS 后端。

## 测试

```powershell
cargo test
```

标签生成与匹配逻辑（`hint.rs`）有独立单元测试，不依赖 Windows API。

## 许可证

本项目以 GNU General Public License v2.0（GPL-2.0-only）发布，完整条款见 [LICENSE](LICENSE)。
