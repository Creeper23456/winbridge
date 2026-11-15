<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

[English](README.md) | **简体中文**

**面向 QEMU/KVM 上 Windows 11 的 Linux GTK4 查看器与客户机代理。**

WinBridge 在 GTK4 窗口中显示 Windows 11 客户机桌面,并让两端保持同步:客户机分辨率跟随窗口大小(没有黑边),客户机 DPI 缩放跟随 Linux 的缩放因子,文本剪贴板双向共享,文件和文本可以拖放到窗口中。鼠标位于画布上时,显示的是客户机光标,而不是 Linux 指针。

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
| 用客户机光标代替 Linux 指针 | 画布上隐藏 Linux 指针;如果客户机发送了硬件光标图像,则显示该图像 |

> 说明:以上验证结果对应拖放功能改动之前的版本。下面"已知限制"中关于拖放去向的描述属于新行为,以及"设备"菜单(光盘镜像、USB 直通),都还没有在客户机上做端到端验证。

已知限制:

- **DPI 受分辨率限制。** Windows 只提供能保留可用有效分辨率(高度约 700 px 以上)的缩放档位。在 1920x888 下最高只有 125%,所以 200% 的请求会失败。在 HiDPI 显示器上,画布的物理像素很大,可以达到 200%。**200% 没有做过端到端测试**(没有 HiDPI 显示器);100% 和 125% 上限已测试。
- **只支持整数缩放因子。** X11 上的 GTK 报告的是整数 `scale_factor`,看不到 Linux 的分数缩放。
- **Windows 的缩放档位是离散的**(100、125、150、175、200 ……),请求值会被取整到最近的档位。
- **剪贴板只支持文本。** 协议里定义了 PNG/BMP 格式,但查看器和代理都没有同步图像。
- **拖放只支持主机→客户机。** 文件按客户机里鼠标落点的位置分发:桌面或 Explorer 中的文件系统文件夹直接接收文件;其他应用通过 OLE `CF_HDROP` 拖放接收,文件先暂存在 `%USERPROFILE%\Downloads\WinBridge`。目标应用必须接受 OLE 文件拖放;被拒绝或失败的拖放,文件会保留在暂存目录。不支持目录、客户机→主机的拖放,也不支持原生文本拖放(拖入的文本会作为 `.txt` 文件保存到暂存目录)。已存在的文件绝不会被覆盖,而是追加 ` (1)`、` (2)` ……后缀。把文件拖到 Explorer 窗口里的某个文件夹图标上时,目标是该窗口当前的文件夹,而不是那个图标。
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

2. 启动查看器:`winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock --qmp-socket /tmp/winbridge-qmp.sock`。标题栏右上角的 **设备(Devices)** 菜单可以插入/弹出 CD/DVD 镜像,并把主机的 USB 设备直通给 Windows。USB 两个方向都支持热插拔,已勾选的设备在重新连接后会自动重新挂载。直通期间,设备会从 Linux 主机驱动上分离;运行查看器的用户需要通过 udev 规则或用户组获得 `/dev/bus/usb/*` 的访问权限。
3. 在客户机里用管理员权限的 PowerShell 安装代理:`scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`。

代理**必须运行在已登录用户的会话中,并具有管理员权限**(安装脚本会注册一个登录时触发、最高权限的计划任务)。打开 `\\.\Global\org.winbridge.0` 需要管理员权限;受限令牌的进程会无提示地一直重试。查看器和代理可以各自独立重启。

客户机需要 virtio-win 驱动(`viogpudo`、`vioser`)。用 `RUST_LOG=info` 运行代理可以看到它的行为。

## 输入

所有鼠标按键(左、中、右、后退、前进)和滚轮(两个方向)都会转发。只有在查看器获得焦点、且鼠标位于画布内时,查看器才会在 X11 上抓取键盘、在 Wayland 上抑制系统快捷键,这样 Super(Windows 键)、Alt+Tab 等桌面快捷键才会交给 Windows。鼠标移出画布或切换到其他窗口时,会释放抓取,并释放客户机中仍被按住的按键;此范围之外的按键事件照常传递,所以 Linux 桌面和 Flatpak 应用的快捷键不受影响。

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
