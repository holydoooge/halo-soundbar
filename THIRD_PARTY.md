# 第三方组件与素材来源

Halo Soundbar 是独立的本地控制工具，与漫步者（EDIFIER）无关联。

## 依赖

- `windows-reactor` 及其工作区依赖来自 Microsoft 的 `windows-rs`，固定提交为 `f5f7418c950e18a52a93d5af211d55050f860383`。
- 其他 Rust 依赖及准确版本列于 `Cargo.toml` 和 `Cargo.lock`。各依赖的许可证以其上游发布内容为准。
- 生成图标的可选 Python 工具使用 Pillow，运行音箱控制程序不需要 Python。

## 图标

- `assets/app-icon.svg`、`assets/app-icon.png`、`assets/app.ico` 是本项目独立绘制的应用图标，不使用 EDIFIER 官方应用标志。
- `assets/big_effect_*.png` 和 `assets/big_eq_*.png` 是此前从 EDIFIER 官方客户端资源整理的灯效和音效卡片参考图。其权利属于各自权利人，不属于本项目独立绘制的应用图标。

## 协议资料

`docs/PROTOCOL.md` 记录 Halo Soundbar 本地互操作研究及真机验证结果。
官方安装包、提取的程序模块、反汇编文件、解密产品列表和本地研究工具不包含在这个源码仓库中。
