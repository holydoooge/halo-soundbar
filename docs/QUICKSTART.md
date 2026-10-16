# 快速上手

## 连接

在 [GitHub Releases](https://github.com/holydoooge/halo-soundbar/releases/latest) 的 Assets 中下载 Windows x64 exe。运行需要 Windows App Runtime 2（2.5.1.0 或兼容更新版本，x64）；缺少时请按启动提示，从 [微软官方下载页](https://learn.microsoft.com/windows/apps/windows-app-sdk/downloads) 安装稳定版 x64 Runtime。

使用 USB 连接 **EDIFIER Halo Soundbar**，启动 `halo-soundbar.exe`。
标题栏显示“已连接”后即可使用。本程序仅识别这一款设备。

如果显示“正在连接…”，检查音箱电源和 USB 连接，并关闭其他控制客户端后等待自动重连。

## 控制台

1. 拖动音量滑条调整音量。
2. 选择音乐、游戏或电影音效模式。
3. 打开氛围灯，选择一种灯光效果。
4. 调整效果支持的亮度和速度；不支持的参数会禁用并说明原因。
5. 在调色板选择颜色，然后点击“应用此颜色”。选色过程不写入音箱，切换灯效会丢弃未应用的颜色。

显示屏与音箱灯光的硬件不同，屏幕显示可能与实际灯光有色差，以实际灯光为准。
幻彩潮汐和炫彩涟漪使用自带配色，不支持自定义颜色。

## 设置

在“外观 → 主题”中选择跟随系统、浅色或深色。切换立即生效，重启后仍保留选择；主题只影响本软件。

可查看软件和固件版本。恢复默认灯效、恢复出厂与关机会弹出确认对话框。
恢复出厂会清除设备端设置；关机后需使用音箱电源键重新开机。

## 构建

安装 Rust MSVC 工具链（1.95+）、Visual Studio C++ Build Tools 与 Windows SDK，然后在项目根目录运行：

```powershell
powershell -ExecutionPolicy Bypass -File .\setup.ps1
.\target\release\halo-soundbar.exe
```

依赖由 Cargo 自动下载。只需要分发编译后的 exe，不需要附带 `assets/`。

## 只读检查

```powershell
cargo run --locked --bin halo-diag -- read
```

如果设备暂时不应答，先停止频繁操作，等待自动恢复。
程序会自动同步设备状态；轮询间隔不等于每次操作的完成时间，灯效写入可能需要约 900 ms。

[构建与测试命令](../README.md) · [验证范围](TESTING.md)
