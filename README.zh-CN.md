<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# WinBridge

[English](README.md) | **简体中文**

**面向 QEMU/KVM 上 Windows 11 的 Linux GTK4 查看器与客户机代理。**

WinBridge 在 GTK4 窗口中显示 Windows 11 客户机桌面,并让两端保持同步:客户机分辨率跟随窗口大小(没有黑边),客户机 DPI 缩放跟随 Linux 的缩放因子,剪贴板(文本、HTML、RTF、图片、文件)双向共享,文件、文件夹和文本可以在两端之间拖放。鼠标位于画布上时,显示的是客户机光标,而不是 Linux 指针。客户机里的程序可以使用主机 GPU 运行 Vulkan、Direct3D 9/10/11 和 OpenGL([3D 加速](#3d-加速)),控制和大数据流量也可以改走共享内存,不再经过 virtio-serial。

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
- **共享内存(可选,`shm = true`)**:一个 `ivshmem` 设备承载控制通道和大数据通道(剪贴板、拖放、文件流),两条通道有优先级,所以大文件粘贴不会让控制消息排在它后面。virtio-serial 仍用于握手和回退([设计与实测](docs/IVSHMEM-TRANSPORT.md),目前为英文)。
- **显示路径**:QEMU 提供 `Listener.Unix.Map` 时,查看器以只读方式映射客户机扫描输出的内存并直接从中绘制,不发送、也不复制任何像素;GTK 的重绘由更新驱动,而不是由合成器节拍驱动(见[速度与诊断](#速度与诊断))。

## 3D 加速

客户机里的程序可以使用主机 GPU。WinBridge 把 Mesa 的 **Venus**(一个 Vulkan 序列化层)移植到了 Windows:客户机驱动 `vulkan_virtio.dll` 把每个 Vulkan 调用编码成命令流,命令流经共享内存送到主机,`winbridge-venus` 再用 virglrenderer 的 Venus 渲染器在主机 GPU 上执行。设计、被否决的备选方案和全部实测数据见 [ADR 0001](docs/adr/0001-venus-as-serialization-layer.md);构建、安装和运行见 [`venus/README.md`](venus/README.md)(均为英文)。

- **能运行什么**:Vulkan 程序;通过 DXVK 运行的 Direct3D 9、10、11;通过 Zink 运行的 OpenGL 4.6;用同一批 DLL 的 x86 版本运行的 32 位程序;FurMark 2 的 Vulkan 模式和 OpenGL 模式。Vulkan 不需要为每个程序做任何事。OpenGL 和 Direct3D 需要把它们的 DLL 加载进程序:放在程序旁边,或者安装**全局 hook**(`scripts/install-gfx-hook.ps1`,一个 AppInit_DLLs 模块),它把这些 DLL 交给每一个不属于 Windows 的程序(黑名单除外),不改动程序自己的文件夹。已测试:OpenGL 和 Direct3D 测试程序(x64 和 x86)和 FurMark 2 的一份拷贝(OpenGL 4.6,约 230 fps)。够不到的:有沙盒的程序,所以浏览器需要 `--use-angle=vulkan`(Edge 用这个参数跑过),以及开启 Secure Boot 时的全部程序。每个创建 DXGI 工厂的程序在运行期间都会占用一个 Venus 槽位;槽位少的时候安装前请看 [`venus/README.md`](venus/README.md) 和 ADR 3.11。
- **实测**(Windows 11 LTSC 客户机,4 vCPU,RTX 3060 Ti 主机,普通用户令牌):D3D11 三角形每帧 1.9 ms,茶壶场景在 1280x720 下每帧 2.0 ms(约每秒 500 帧);运行 16 分钟没有泄漏,主机端也没有警告;FurMark 的 Vulkan 模式约 100 FPS。Direct3D 10(2.35 ms)、Direct3D 9 以及 32 位版本(三角形和茶壶 2.5 到 3.2 ms)也都跑通了。
- **没有中转进程**:每个 Vulkan 进程独占一个*槽位*,即一个 64 MiB 的 `ivshmem-plain` 设备加一个 virtio-serial 唤醒端口,由一个 `winbridge-venus` 进程为它服务。示例启动脚本用 `WINBRIDGE_VENUS=N` 启动这些槽位。
- **主机需要**:带有 `venus/patches/virglrenderer` 补丁的 virglrenderer(NVIDIA 550 上其中两个是必需的)及其 render server。
- **在 Linux 上构建**:`WBVN_ACCEPT_MS_LICENSE=1 venus/build/build.sh` 用 `clang-cl` 交叉编译驱动、OpenGL DLL 和 x86 版本;客户机内不编译任何东西。`scripts/install-venus-icd.ps1` 负责注册驱动(需要先装好 ivshmem 驱动)。
- **开销**:主机上的 `winbridge-venus` 会有意地忙等占满一个核心。茶壶场景在 720p 下做 15 秒采样:查看器占一个核心的 45%,QEMU 占 202%,Venus 线程占 100%。
- **限制**:每个槽位同时只服务一个 Vulkan 进程,该进程的多个 Vulkan 实例共用这个槽位(DXVK 和 FurMark 的 Vulkan 模式都会创建两个)。OpenGL 交换间隔保持为 1 的程序会被限制在每帧约 14.7 ms。测试程序和 FurMark 之外的应用没有运行过。没有 `winbridge-venus` 时,Vulkan 程序最多会等主机 30 秒。

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
| 3D:Vulkan、Direct3D 9/10/11(DXVK)、OpenGL(Zink)、x64 和 x86、FurMark 2 Vulkan | 在 LTSC 虚拟机中用测试程序和 FurMark 验证正常;其他应用没有运行过 |
| 控制和大数据流量的共享内存传输(`shm = true`) | 正常;在构建虚拟机里,主机→客户机 2.0 到 3.4 GiB/s,virtio-serial 为 239 MiB/s;大数据流占满时控制消息往返仍为 p50 135 到 150 us(serial 为 p50 4.2 ms) |
| 来自共享内存扫描输出的显示,查看器里没有拷贝 | 正常(帧缓冲拷贝 12.5 ms → 0.0 ms;GDI 程序和 Vulkan 程序下每秒约 60 次显示更新) |
| 文件夹拖放(打包成一个 zip),双向 | 主机→客户机已在 LTSC 虚拟机里用无界面的 `folderdnd` 驱动验证(嵌套目录、空目录、非 ASCII 名称和一个 5 MiB 文件,SHA-256 全部一致;Windows 无法创建的名字被省略并列出;第二次拖放落成 `name (1)`);GTK 的拖放手势和客户机→主机没有运行过 |

已知限制:

- **DPI 受分辨率限制。** Windows 只提供能保留可用有效分辨率(高度约 700 px 以上)的缩放档位。在 1920x888 下最高只有 125%,所以 200% 的请求会失败。在 HiDPI 显示器上,画布的物理像素很大,可以达到 200%。**200% 没有做过端到端测试**(没有 HiDPI 显示器);100% 和 125% 上限已测试。
- **只支持整数缩放因子。** X11 上的 GTK 报告的是整数 `scale_factor`,看不到 Linux 的分数缩放。
- **Windows 的缩放档位是离散的**(100、125、150、175、200 ……),请求值会被取整到最近的档位。
- **剪贴板:文本、HTML、RTF、图片和文件,双向。** 文本、HTML、RTF 和图片(单项不超过 48 MiB)在复制时同步。复制的文件**不会**在复制时传输:直到你粘贴(Ctrl+V)那一刻才开始,画布顶部出现进度条,传输期间虚拟机输入被锁定(客户机继续运行),文件到达粘贴目标(每次粘贴上限 8 GiB)。虚拟文件来源(如压缩包管理器、邮件附件)两侧都不支持。*共享逻辑做了单元测试,客户机的延迟渲染在构建虚拟机里测过;还没有端到端测试。*
- **拖放支持双向。** 主机→客户机:由鼠标位置决定目标。松开左键那一刻开始接收,先存到客户机里的私有暂存目录,再**移动**到鼠标下的 Explorer 文件夹或桌面(其他位置落到 `Downloads\WinBridge`);拖入的文本会保存为 `.txt` 文件。客户机→主机:从 Windows 应用里开始拖动,按住左键把鼠标移出画布,鼠标下的 Linux 应用就会收到文件,最终放在哪里由它决定(只有在放下时才从客户机取文件;上限 8 GiB)。文件夹打包成一个 zip 传输(打包、发送、解包、删除四步合起来填满进度条);另一侧无法创建的条目会被省略,并在提示里列出。每次传输都会显示进度条并锁定输入。只有磁盘上存在的文件、文件夹和文本可以拖出;虚拟文件(压缩包、邮件附件、浏览器下载)不行。已有的文件和文件夹不会被覆盖。"文件还在传输时就把路径交给应用"没有实现。*还没有通过查看器端到端验证;客户机→主机依赖客户机的 OLE 拖动在 700 ms 内询问探测窗口,这一点还没有观察到。主机→客户机的文件夹拖放已在虚拟机里通过查看器的协议、用无界面驱动(`folderdnd`)运行过:嵌套目录、空目录、非 ASCII 名称和一个 5 MiB 文件的 SHA-256 全部一致,Windows 无法创建的名字被省略并列出,第二次拖放落成 `name (1)`。GTK 的拖放手势和客户机→主机的 zip 没有在那里运行过(zip 代码有单元测试)。*
- **很大或非常规的分辨率**需要足够的 virtio-gpu 显存。
- **客户机显示以 64 Hz 运行,而不是主机显示器的刷新率。** 查看器会请求主机所有显示器中最低的刷新率(最高 60 Hz);virtio-gpu 的 display-only 驱动保持 64 Hz 并拒绝(查看器日志里是 `Guest did not take the refresh rate 60000 mHz ... it runs at 64000 mHz`)。GDI 程序每秒约有 60 次显示更新,通过 Venus 驱动呈现的程序也是如此(这是在驱动使用 1 ms 定时器分辨率时测得的;更早的构建请求 0.5 ms,会让整个桌面降到每秒约 33 次更新,见 `docs/adr/0001-venus-as-serialization-layer.md` 里的 LTSC 记录)。
- **云母(Mica)和亚克力(acrylic)默认是关的,要告诉 DWM 才会开。** DWM 在只有显示功能的 virtio-gpu 适配器上用 Basic Render Driver 合成(`dxdiag`:WDDM 1.3,Direct3D 0/4),画出的是不透明的回退颜色:一个请求云母或亚克力的测试窗口(`DwmSetWindowAttribute` 成功并能读回数值)显示为纯灰色。把 `HKLM\SOFTWARE\Microsoft\Windows\Dwm\ForceEffectMode` 设为 2(DWM 会读这个值,名字在 `uDWM.dll` 和 `dwmcore.dll` 里,2 表示"开启"是试出来的)并重启 DWM(`Stop-Process dwm`,Windows 会自动再启动它,或者注销重登),同一个测试里亚克力有了真实的模糊,云母成了带壁纸色调的材质;开启后 DWM 空闲 8 秒用了 0.13 秒 CPU。没有测量:窗口移动时 DWM 的开销、对每秒 60 次显示更新的影响,以及重启后是否保持。删除这个值并重启 DWM 即可撤销。

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
   -smp 8,sockets=1,cores=8,threads=1
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

   默认是 **8 个 vCPU**,单插槽(`WINBRIDGE_VCPUS=N` 可改)。Windows 在 CPU 上合成桌面(WARP,见下文"限制"):窗口移动时在 4 vCPU 的客户机上约占满 2 个核,4 个 vCPU 留给程序的很少。启动脚本和示例 VM 用 8;8 相对 4 对帧时间的影响还没有测过。客户机只看到一个插槽,因为 Windows 客户端版本最多支持两个插槽。

2. 启动查看器:`winbridge-host --socket /tmp/winbridge.sock --dbus-addr unix:path=/tmp/qemu-dbus.sock --qmp-socket /tmp/winbridge-qmp.sock`(或者见下面的"启动脚本与生命周期")。标题栏右上角的 **设备(Devices)** 菜单可以插入/弹出 CD/DVD 镜像,并把主机的 USB 设备直通给 Windows。USB 两个方向都支持热插拔,已勾选的设备在重新连接后会自动重新挂载。直通期间,设备会从 Linux 主机驱动上分离。**键盘和鼠标永远不会被列出,也没有覆盖选项**(任何带键盘或鼠标 HID 接口的设备都会被过滤,包括复合设备),避免把你锁在主机之外。运行查看器的用户需要访问 `/dev/bus/usb/*`:运行 `scripts/install-host-udev.sh`(`--dry-run` 可预览)来授权。
3. 在客户机里用管理员权限的 PowerShell 安装代理:`scripts\install-agent.ps1 -SourceBinary .\winbridge-agent.exe`。脚本先检查文件(64 位 Windows 程序、是 WinBridge 代理、能运行),客户机没有可用的 virtio 串口设备时给出警告,把安装目录限制为只有 SYSTEM 和管理员可写(服务会运行里面的程序),旧版本保留为 `winbridge-agent.previous.exe`;服务起不来或代理没有在控制台会话里启动时,**会恢复原状并以 1 退出**。再次运行即更新。`-Status` 显示状态(服务、程序哈希、进程、日志、共享内存设备),不改任何东西;`-Uninstall` 移除服务(文件保留,加 `-Purge` 还会删除程序和 `%ProgramData%\WinBridge`);`-InstallDir` 和 `-StartTimeoutSeconds` 改目录和等待时间。

代理**以 SYSTEM 身份运行**。安装脚本注册服务 `WinBridgeAgent`(开机自启,LocalSystem),由它在控制台会话里、用已登录用户的环境变量启动代理,登录界面就已经启动;代理结束或换了用户登录时服务会再次启动它。打开 `\\.\Global\org.winbridge.0` 需要管理员权限;受限令牌的进程会无提示地一直重试。处理用户文件的代码以用户身份运行(相应线程会模拟已登录用户),所以拖入和粘贴的文件属于用户,落在用户自己的目录里。服务日志在 `%ProgramData%\WinBridge\service.log`,代理日志在已登录用户的 `%LOCALAPPDATA%\WinBridge\agent.log`。查看器和代理可以各自独立重启。

客户机需要 virtio-win 驱动(`viogpudo`、`vioser`)。用 `RUST_LOG=info` 运行代理可以看到它的行为。

### 速度与诊断

- `--shm`(或 `shm = true`)开启共享内存传输。它需要 ivshmem 设备(示例脚本 `scripts/launch-qemu-winbridge.sh` 里设 `WINBRIDGE_SHM=1`)、客户机里的 ivshmem 驱动,以及连同它一起安装的代理;缺少任何一项都不会改变现有行为。
- 查看器在 GTK 启动前设置 `GDK_DEBUG=no-vsync`。在实测的 X11 GNOME 会话(显示器 75 Hz 和 60 Hz)上,GTK 的帧时钟中位间隔为 33 ms,客户机每秒送来 60 次更新却只显示 30 次。`--vsync` 恢复 GTK 自己的节拍。
- `--stats` 每秒记录一次显示路径的开销(更新次数、处理耗时、重绘次数、帧缓冲拷贝耗时、更新之间的间隔),并在你操作鼠标或键盘时记录发给 QEMU 的输入调用次数以及 QEMU 的应答耗时。
- `winbridge-venus --stats` 记录它的循环,以及它比较了多少字节的客户机内存,按环、其他共享内存和设备内存分开统计。
- 不带 GTK、用真实代理扮演查看器的示例:`shme2e`(传输吞吐和延迟)和 `folderdnd`(一次文件夹拖放),在 `crates/winbridge-host/examples/` 里。运行它们时查看器不能处于连接状态。

### 启动脚本与生命周期

虚拟机怎么启动由你自己管理。把查看器指向一个脚本,它会原样执行一次,并把脚本作为自己的子进程:

```bash
winbridge-host --launch-script ~/vm/start-windows.sh
```

或写在 `~/.config/winbridge/config.toml` 中(键:`launch_script`、`launch_args`、`socket`、`dbus_addr`、`qmp_socket`、`usb_bus`、`grab_keyboard`、`shm`;命令行优先;未知键会报错):

```toml
launch_script = "/home/me/vm/start-windows.sh"
```

脚本的环境里有 `WINBRIDGE_SOCK`、`WINBRIDGE_DBUS_ADDR`、`WINBRIDGE_QMP_SOCK`,输出追加到 `~/.local/state/winbridge/launch.log`。[`scripts/launch-qemu-winbridge.sh`](scripts/launch-qemu-winbridge.sh) 是遵守这些变量的示例。没有配置启动脚本时,查看器只连接你已经启动的虚拟机。脚本在虚拟机可连接之前失败,会弹出带日志路径的错误;虚拟机运行过之后脚本正常退出,查看器随之关闭。

查看器的生命周期等于虚拟机的生命周期。关闭窗口时会询问:**关闭虚拟机**(优雅的 ACPI 关机,不会杀任何进程)、**最小化到托盘**、**取消**,没有"仅关闭查看器"。托盘图标(StatusNotifierItem)可以显示/隐藏窗口并关闭虚拟机;GNOME 需要 AppIndicator/KStatusNotifierItem 扩展,否则"最小化到托盘"只会最小化。如果客户机在内部被关机,或 QEMU 已退出,查看器会在标题栏显示"Guest is off",此时关闭窗口会直接退出,不再弹出关机对话框。QEMU 同一时间只服务一个 QMP 客户端,所以查看器每条命令单独连接,把套接字留给其他工具。通过 virt-manager/libvirt 启动配置档暂未实现。

## 输入

所有鼠标按键(左、中、右、后退、前进)和滚轮(两个方向)都会转发。**输入捕获严格按鼠标位置划定范围:** 只有当窗口处于激活状态、且鼠标位于虚拟机画布内时,键盘和鼠标才会交给客户机。鼠标在标题栏、标题栏上的按钮区、窗口之外,或者焦点在其他窗口时,什么都不会被捕获,客户机中仍被按住的按键也会被释放;范围之外的按键事件照常传递。在画布上时,Linux 指针会被隐藏,只显示 Windows 光标。鼠标事件通过一个有序队列送达 QEMU,每一次按键或松开之前,都会先发送同一事件对应的准确指针位置,因此点击会落在指针所在的位置。

**手动释放鼠标。** Ctrl+Alt+G,或单独轻点右 Ctrl(按下并松开,中间没有按其他任何键),会在鼠标位于画布上时释放鼠标和键盘,例如客户机卡死时。这两个快捷键完全在主机端处理:不需要 `--grab-keyboard`,客户机卡死时和文件传输期间同样有效;客户机永远看不到 G 键,并且会先释放已经发给客户机的 Ctrl/Alt。右 Ctrl 只是被暂存到下一次按键为止,因此把右 Ctrl 用作修饰键的客户机快捷键仍然正常工作。释放后显示 Linux 指针,不转发任何事件,标题栏显示 `Mouse released`。释放状态会一直保持,直到你再次按下任一快捷键、点击画布(该次点击不会发送给客户机),或者把指针移出画布再移回来。标题栏只会在客户机代理约 5 秒没有响应心跳(或虚拟机运行时代理通道断开)时,以及在游戏模式下,显示快捷键提示(`Ctrl+Alt+G or Right Ctrl: release mouse`)。

默认情况下查看器**不会**主动抓取键盘,因为抓取会把整个会话的按键都拿走,包括其他应用的输入法(测试中 Flatpak 应用因此失去了输入)。能到达画布的按键会被转发,但 Super(Windows 键)、Alt+Tab 这类桌面快捷键仍归桌面。如需让这些快捷键进入 Windows,用 `--grab-keyboard`(或在 `config.toml` 中写 `grab_keyboard = true`)主动开启:开启后,鼠标在画布内时,查看器在 X11 上抓取键盘、在 Wayland 上抑制系统快捷键。

**游戏模式(相对鼠标),自动进入。** 没有按钮。只要有不属于 Windows 的程序在使用 3D 加速(它加载了 Venus 驱动;客户机代理报告这类程序的列表,最后一个结束时列表清空),画布就会切换为相对运动,用于需要原始位移量的游戏(第一人称视角等)。鼠标位于画布上时,指针会被隐藏并固定在画布中心,移动以 `RelMotion` 位移量的形式发送。每次鼠标指针进入游戏模式时,会短暂显示一个小徽标(鼠标图标和 "Game mode")。用 Ctrl+Alt+G 或右 Ctrl 释放(规则同上);失去焦点、文件传输、关机和虚拟机停止时也会释放。*还没有在虚拟机里端到端运行过。* 需求与限制如下,直说:

- QEMU 一次只把指针输入交给一个设备,而当这个设备是绝对设备(tablet)时会拒绝 `RelMotion`。因此游戏模式开始时,查看器会通过 QMP 套接字用监视器命令 `mouse_set` 让一个相对设备成为当前设备(`info mice` 列出设备),在游戏模式结束时和查看器启动时再把 tablet 设为当前设备。q35 机型总带有 PS/2 鼠标,它是相对设备;`-device usb-mouse` 也可以(`scripts/launch-qemu-winbridge.sh` 在使用 `WINBRIDGE_REL_MOUSE=1` 启动时会添加它)。如果选不出相对设备,或 QEMU 仍然拒绝相对运动,游戏模式会保持关闭,直到这些程序结束,标题栏会说明原因。Windows 是否从 PS/2 鼠标接收移动,这里还没有验证。
- 没有相对设备时,自己把光标拉回中心的游戏(大多数第一人称游戏)无法通过 tablet 工作:主机无法跟随客户机自己造成的光标跳转。
- 需要 **X11**。GTK4 在 Wayland 上没有指针锁定 API,所以游戏模式在那里不会启动,标题栏会说明原因,而不是伪造实现。在 X11 上,指针一旦偏离就会被拉回画布中心;没有指针或键盘抓取,所以非常快的甩动仍可能离开画布,离开时照常释放捕获。
- 游戏模式只跟随 3D 程序,别的什么都不跟随,捕获也和其他捕获一样按鼠标位置划定范围:鼠标不在画布上,或者手动释放之后,客户机的指针照常工作。如果 QEMU 拒绝相对运动,它会一直保持关闭,直到这些程序结束,标题栏会说明原因。

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
