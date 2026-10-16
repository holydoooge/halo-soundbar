# Halo Soundbar

<img src="assets/app-icon.png" width="96" alt="Halo Soundbar 独立应用图标">

为 **EDIFIER Halo Soundbar（花再 Halo 音箱）**制作的 Windows 本地控制面板，使用 Rust + WinUI 3。
仅通过 USB HID 控制设备，运行时不联网、不需要登录官方客户端。

**只适配 VID `2D99` / PID `A063` 的 Halo Soundbar，不支持其他漫步者型号。**

## 功能

- 音量调节、音乐 / 游戏 / 电影三个音效模式。
- 七种灯光效果、氛围灯总开关，以及效果支持的亮度、速度调节。
- RGB / HSV 调色板：选色后点击“应用此颜色”才写入音箱。屏幕与实际灯光可能因硬件差异而存在色差。
- 设备状态自动同步和断线重连。
- 固件版本显示、恢复默认灯效、恢复出厂和关机确认对话框。
- 独立应用图标，设置页可选择跟随系统、浅色或深色主题，立即生效并记住选择。

蓝牙控制和自定义 EQ 不在当前实现范围内。验证范围与未测场景见 [测试记录](docs/TESTING.md)。

## 构建与运行

需要 Windows、Rust **1.95 或更新版本的 MSVC 工具链**、Visual Studio C++ Build Tools 与 Windows SDK。
首次构建需要联网下载 Cargo 依赖，音箱控制程序本身不联网。

```powershell
git clone https://github.com/holydoooge/halo-soundbar.git
cd halo-soundbar
powershell -ExecutionPolicy Bypass -File .\setup.ps1
.\target\release\halo-soundbar.exe
```

也可以直接构建：

```powershell
cargo build --locked --release --bin halo-soundbar
```

`windows-reactor` 固定到 `microsoft/windows-rs` 的具体提交，由 Cargo 自动获取；无需手动复制本地 `windows-rs` 目录。

构建后的 `target/release/halo-soundbar.exe` 已内嵌图标，可复制到其他目录运行，无需附带源码和 `assets/`。
构建产物不提交到 Git 仓库。

## 验证

```powershell
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
```

普通测试不修改设备设置。需要连接真机、临时修改并恢复普通设置的测试，必须主动运行：

```powershell
cargo test --locked --test hardware -- --ignored --nocapture
```

运行真机测试前，请打开氛围灯并关闭其他音箱控制客户端。它不执行关机或恢复出厂。

只读诊断：

```powershell
cargo run --locked --bin halo-diag -- read
```

`halo-diag write`、`halo-diag census` 和 `census` 是开发用写入/协议探测工具，会发送设备命令；日常使用只需 GUI。

## 项目结构

```text
src/           界面、设备会话、HID 通信与 EC 协议
assets/        编译进程序的图标和图片
tests/         可选的真机往返测试
tools/         独立应用图标生成脚本
docs/          使用说明、协议与测试记录
setup.ps1      构建及可选检查入口
Cargo.lock     锁定应用依赖版本
```

- [快速上手](docs/QUICKSTART.md)
- [软件设计与模块说明](docs/SOFTWARE.md)
- [Halo Soundbar 控制协议](docs/PROTOCOL.md)
- [测试与修复记录](docs/TESTING.md)
- [第三方组件与素材来源](THIRD_PARTY.md)

## 图标

应用图标是本项目独立绘制的音箱与声波图案。可编辑文件在 `assets/app-icon.svg`，ICO 包含 16–256 像素尺寸。
修改生成脚本后，可用 Python + Pillow 重新生成：

```powershell
python -m pip install -r tools/requirements.txt
python tools/create_app_icon.py
```

## 项目说明

本项目用于与已购买的 Halo Soundbar 进行本地互操作，与漫步者（EDIFIER）无关联。
第三方库和参考素材的来源见 [THIRD_PARTY.md](THIRD_PARTY.md)。
