<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

[English](README.md) | **简体中文**

**面向 QEMU/KVM 上 Windows 11 的 Linux GTK4 查看器与客户机代理。**

WinBridge 在 GTK4 窗口中显示 Windows 11 客户机桌面,并让两端保持同步:客户机分辨率跟随窗口大小(没有黑边),客户机 DPI 缩放跟随 Linux 的缩放因子,剪贴板(文本、HTML、RTF、图片、文件)双向共享,文件和文本可以在两端之间拖放。鼠标位于画布上时,显示的是客户机光标,而不是 Linux 指针。

做这个项目的原因:QEMU 自带的 GTK 窗口无法调整使用 virtio-gpu(`viogpudo`)驱动的 Windows 客户机的分辨率,驱动知道了新尺寸,却没有任何东西去应用它。WinBridge 向 QEMU 请求新尺寸,再由客户机内部的代理自己应用。

## 工作原理

```
 GTK4 查看器 (winbridge-host)                        Windows 客户机
 ┌──────────────────────────┐   -display dbus   ┌───────────────────────────┐
 │ 画布、输入、光标          │◄─────────────────►│ QEMU virtio-gpu           │
 │ 窗口尺寸 + 缩放 ─────────┼─ SetUIInfo ──────►│ 驱动生成自定义显示模式     │
 │                          │   virtio-serial   │ winbridge-agent           │
 │ 剪贴板、拖放、DPI ───────┼──org.winbridge.0─►│ SetDisplayConfig、DPI、   │
 └──────────────────────────┘                   │ 剪贴板、文件接收           │
                                                └───────────────────────────┘
```

- **画布、输入、光标**:使用 QEMU 的 `-display dbus`;查看器在 `org.qemu.Display1.Console_0` 上注册监听器。QEMU 连接的是 D-Bus *总线*,因此需要一个总线守护进程(启动脚本会启动一个私有总线)。
- **分辨率**:查看器用画布的物理像素尺寸(逻辑尺寸 × 缩放因子)调用 `SetUIInfo`,然后让代理通过 `SetDisplayConfig` 应用(与 virtio-win 辅助程序使用的调用相同)。`ChangeDisplaySettings` 只作为后备,因为 Windows 只会给它一组固定的模式列表。
- **DPI**:在分辨率之后发送(Windows 会按分辨率限制缩放比例)。代理会回读实际生效的 DPI,并报告真实结果。
- **通道**:专用 virtio-serial 端口 `org.winbridge.0` 上带版本号、带长度前缀的帧([协议](docs/PROTOCOL.md)、[架构](docs/ARCHITECTURE.md),目前为英文)。它不会占用 `org.qemu.guest_agent.0`。

## 状态

已端到端验证(Windows 11 LTSC 2024 客户机,virtio-win 0.1.285 显卡驱动,QEMU 10.0):

| 功能 | 结果 |
|---|---|
| 画布显示在 GTK 窗口中 | 正常 |
| 客户机分辨率跟随窗口大小 | 正常(例如 1044x661、1280x677、1920x888) |
| DPI 同步 | 在 Windows 允许的范围内正常;超过上限的请求会被报告为失败 |
| 剪贴板,主机→客户机、客户机→主机,文本 | 正常,没有回环 |
| 把文件拖放到窗口 | 正常(3 MiB 文件,SHA-256 一致) |
| 把文本拖放到窗口 | 正常(已验证 UTF-8) |
| 剪贴板的 HTML、RTF、图片、文件;从 Windows 拖到 Linux | 已实现,**还没有端到端验证** |
| 用客户机光标代替 Linux 指针 | 画布上隐藏 Linux 指针;如果客户机发送了硬件光标图像,则显示该图像 |

> 说明:以上验证结果对应拖放功能改动之前的版本。下面"已知限制"中关于拖放去向的描述属于新行为,以及"设备"菜单(光盘镜像、USB 直通),都还没有在客户机上做端到端验证。

已知限制:

- **DPI 受分辨率限制。** Windows 只提供能保留可用有效分辨率(高度约 700 px 以上)的缩放档位。在 1920x888 下最高只有 125%,所以 200% 的请求会失败。在 HiDPI 显示器上,画布的物理像素很大,可以达到 200%。**200% 没有做过端到端测试**(没有 HiDPI 显示器);100% 和 125% 上限已测试。
- **只支持整数缩放因子。** X11 上的 GTK 报告的是整数 `scale_factor`,看不到 Linux 的分数缩放。
- **Windows 的缩放档位是离散的**(100、125、150、175、200 ……),请求值会被取整到最近的档位。
- **剪贴板:文本、HTML、RTF、图片和文件,双向。** 文本、HTML、RTF 和图片(单项不超过 48 MiB)在复制时同步。复制的文件**不会**在复制时传输:直到你粘贴(Ctrl+V)那一刻才开始,画布顶部出现进度条,传输期间虚拟机输入被锁定(客户机继续运行),文件到达粘贴目标(每次粘贴上限 8 GiB)。虚拟文件来源(如压缩包管理器、邮件附件)两侧都不支持。*共享逻辑做了单元测试,客户机的延迟渲染在构建虚拟机里测过;还没有端到端测试。*
- **拖放支持双向。** 主机→客户机:由鼠标位置决定目标。松开左键那一刻开始接收,先存到客户机里的私有暂存目录,再**移动**到鼠标下的 Explorer 文件夹或桌面(其他位置落到 `Downloads\WinBridge`);拖入的文本会保存为 `.txt` 文件。客户机→主机:从 Windows 应用里开始拖动,按住左键把鼠标移出画布,鼠标下的 Linux 应用就会收到文件,最终放在哪里由它决定(只有在放下时才从客户机取文件;上限 8 GiB)。每次传输都会显示进度条并锁定输入。只有磁盘上存在的文件和文本可以拖出;虚拟文件(压缩包、邮件附件、浏览器下载)不行。已有文件不会被覆盖。"文件还在传输时就把路径交给应用"没有实现。*还没有端到端验证;客户机→主机依赖客户机的 OLE 拖动在 700 ms 内询问探测窗口,这一点还没有观察到。*
- **很大或非常规的分辨率**需要足够的 virtio-gpu 显存。

## 构建

### 主机查看器(Linux)

```bash
sudo apt install libgtk-4-dev pkg-config   # Debian / Ubuntu
cargo build --release -p winbridge-host
```

### 客户机代理(Windows,`x86_64-pc-windows-msvc`)

```powershell
cargo build --release -p winbridge-agent
```

代理使用静态 CRT 链接(`.cargo/config.toml`),所以在没有 Visual C++ 运行库的原版 Windows 上也能运行。在 Linux 上做类型检查:`rustup target add x86_64-pc-windows-msvc && cargo check --target x86_64-pc-windows-msvc -p winbridge-agent`。

## 运行

1. 用 D-Bus 显示和 WinBridge 端口启动 QEMU。[`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) 会启动私有总线,并展示所需参数:

   ```
   -display dbus,addr=unix:path=/tmp/qemu-dbus.sock
   -qmp unix:/tmp/winbridge-qmp.sock,server=on,wait=off
   -device qemu-xhci,id=xhci
   -drive if=none,id=cd0,media=cdrom,readonly=on
   -device ide-cd,drive=cd0,id=cdrom0
   -device virtio-gpu-pci,edid=on
   -device virtio-serial-pci
   -chardev socket,id=wb0,path=/tmp/winbridge.sock,server=on,wait=off
   -device virtserialport,chardev=wb0,name=org.winbridge.0
   ```

2. 启动查看器:`winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock --qmp-socket /tmp/winbridge-qmp.sock`(或者见下面的"启动脚本与生命周期")。标题栏右上角的 **设备(Devices)** 菜单可以插入/弹出 CD/DVD 镜像,并把主机的 USB 设备直通给 Windows。USB 两个方向都支持热插拔,已勾选的设备在重新连接后会自动重新挂载。直通期间,设备会从 Linux 主机驱动上分离。**键盘和鼠标永远不会被列出,也没有覆盖选项**(任何带键盘或鼠标 HID 接口的设备都会被过滤,包括复合设备),避免把你锁在主机之外。运行查看器的用户需要访问 `/dev/bus/usb/*`:运行 `scripts/install-host-udev.sh`(`--dry-run` 可预览)来授权。
3. 在客户机里用管理员权限的 PowerShell 安装代理:`scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`。

代理**必须运行在已登录用户的会话中,并具有管理员权限**(安装脚本会注册一个登录时触发、最高权限的计划任务)。打开 `\\.\Global\org.winbridge.0` 需要管理员权限;受限令牌的进程会无提示地一直重试。查看器和代理可以各自独立重启。

客户机需要 virtio-win 驱动(`viogpudo`、`vioser`)。用 `RUST_LOG=info` 运行代理可以看到它的行为。

### 启动脚本与生命周期

虚拟机怎么启动由你自己管理。把查看器指向一个脚本,它会原样执行一次,并把脚本作为自己的子进程:

```bash
winbridge-host --launch-script ~/vm/start-windows.sh
```

或写在 `~/.config/winbridge/config.toml` 中(键:`launch_script`、`launch_args`、`socket`、`dbus_addr`、`qmp_socket`、`usb_bus`、`grab_keyboard`;命令行优先;未知键会报错):

```toml
launch_script = "/home/me/vm/start-windows.sh"
```

脚本的环境里有 `WINBRIDGE_SOCK`、`WINBRIDGE_DBUS_ADDR`、`WINBRIDGE_QMP_SOCK`,输出追加到 `~/.local/state/winbridge/launch.log`。[`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) 是遵守这些变量的示例。没有配置启动脚本时,查看器只连接你已经启动的虚拟机。脚本在虚拟机可连接之前失败,会弹出带日志路径的错误;虚拟机运行过之后脚本正常退出,查看器随之关闭。

查看器的生命周期等于虚拟机的生命周期。关闭窗口时会询问:**关闭虚拟机**(优雅的 ACPI 关机,不会杀任何进程)、**最小化到托盘**、**取消**,没有"仅关闭查看器"。托盘图标(StatusNotifierItem)可以显示/隐藏窗口并关闭虚拟机;GNOME 需要 AppIndicator/KStatusNotifierItem 扩展,否则"最小化到托盘"只会最小化。如果客户机在内部被关机,或 QEMU 已退出,查看器会在标题栏显示"Guest is off",此时关闭窗口会直接退出,不再弹出关机对话框。QEMU 同一时间只服务一个 QMP 客户端,所以查看器每条命令单独连接,把套接字留给其他工具。通过 virt-manager/libvirt 启动配置档暂未实现。

## 输入

所有鼠标按键(左、中、右、后退、前进)和滚轮(两个方向)都会转发。**输入捕获严格按鼠标位置划定范围:** 只有当窗口处于激活状态、且鼠标位于虚拟机画布内时,键盘和鼠标才会交给客户机。鼠标在标题栏、标题栏上的按钮区、窗口之外,或者焦点在其他窗口时,什么都不会被捕获,客户机中仍被按住的按键也会被释放;范围之外的按键事件照常传递。在画布上时,Linux 指针会被隐藏,只显示 Windows 光标。鼠标事件通过一个有序队列送达 QEMU,每一次按键或松开之前,都会先发送同一事件对应的准确指针位置,因此点击会落在指针所在的位置。

**手动释放鼠标。** Ctrl+Alt+G,或单独轻点右 Ctrl(按下并松开,中间没有按其他任何键),会在鼠标位于画布上时释放鼠标和键盘,例如客户机卡死时。这两个快捷键完全在主机端处理:不需要 `--grab-keyboard`,客户机卡死时和文件传输期间同样有效;客户机永远看不到 G 键,并且会先释放已经发给客户机的 Ctrl/Alt。右 Ctrl 只是被暂存到下一次按键为止,因此把右 Ctrl 用作修饰键的客户机快捷键仍然正常工作。释放后显示 Linux 指针,不转发任何事件,标题栏显示 `Mouse released`。释放状态会一直保持,直到你再次按下任一快捷键、点击画布(该次点击不会发送给客户机),或者把指针移出画布再移回来。标题栏只会在客户机代理约 5 秒没有响应心跳(或虚拟机运行时代理通道断开)时,以及在游戏模式下,显示快捷键提示(`Ctrl+Alt+G or Right Ctrl: release mouse`)。

默认情况下查看器**不会**主动抓取键盘,因为抓取会把整个会话的按键都拿走,包括其他应用的输入法(测试中 Flatpak 应用因此失去了输入)。能到达画布的按键会被转发,但 Super(Windows 键)、Alt+Tab 这类桌面快捷键仍归桌面。如需让这些快捷键进入 Windows,用 `--grab-keyboard`(或在 `config.toml` 中写 `grab_keyboard = true`)主动开启:开启后,鼠标在画布内时,查看器在 X11 上抓取键盘、在 Wayland 上抑制系统快捷键。

**游戏模式(相对鼠标),默认关闭。** 标题栏中的 **游戏模式(Game mode)** 开关会把画布切换为相对运动,用于需要原始位移量的游戏(第一人称视角等)。鼠标位于画布上时,指针会被隐藏并固定在画布中心,移动以 `RelMotion` 位移量的形式发送。用 Ctrl+Alt+G 或右 Ctrl 释放(规则同上);失去焦点、文件传输、关机和虚拟机停止时也会释放。需求与限制如下,直说:

- 客户机需要一个**相对**指针设备。QEMU 的 D-Bus 显示接口文档说明:当 `Mouse.IsAbsolute` 为真时 `RelMotion` 会返回错误,因此只有绝对定位 `usb-tablet` 的客户机无法接收相对运动。请在 QEMU 命令行中添加一个,例如 `-device usb-mouse`(`scripts/launch-qemu-winbridge.sh` 在使用 `WINBRIDGE_REL_MOUSE=1` 启动时会添加它;你自己的启动脚本需要同样的设备)。如果 QEMU 拒绝相对运动,游戏模式会自动关闭,并在标题栏说明原因。当 tablet 和 mouse 同时存在时,`IsAbsolute` 报告的是哪一个设备,取决于 QEMU 当前的活动输入处理程序;这一点尚未在此验证。
- 没有相对设备时,自己把光标拉回中心的游戏(大多数第一人称游戏)无法通过 tablet 工作:主机无法跟随客户机自己造成的光标跳转。
- 需要 **X11**。GTK4 在 Wayland 上没有指针锁定 API,因此该按钮在 Wayland 上会被禁用并给出说明,而不是伪造实现。在 X11 上,指针一旦偏离就会被拉回画布中心;没有指针或键盘抓取,所以非常快的甩动仍可能离开画布,离开时照常释放捕获。
- 游戏模式不改变默认的绝对定位行为;不开启时它永远不会生效。

## 许可证

GPL-3.0-or-later,见 [LICENSE](LICENSE)。

```
WinBridge - Linux GTK4 viewer and Windows 11 guest agent for QEMU/KVM
Copyright (C) 2026 Creeper23456

This program is free software: you can redistribute it and/or modify
it under the terms of the GNU General Public License as published by
the Free Software Foundation, either version 3 of the License, or
(at your option) any later version.

This program is distributed in the hope that it will be useful,
but WITHOUT ANY WARRANTY; without even the implied warranty of
MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
GNU General Public License for more details.
```
