# neko player

[简体中文](README.md) | [English](README_EN.md)

[![Build](https://github.com/Yuel25/neko-player/actions/workflows/build.yml/badge.svg)](https://github.com/Yuel25/neko-player/actions/workflows/build.yml)

一款使用 Rust、Slint 和 libmpv 构建的现代 Windows 媒体播放器。

![neko player icon](assets/neko-player-icon.png)

## 界面预览

### 初始界面

![neko player 初始界面](assets/screenshots/home.png)

### 视频播放

![neko player 视频播放界面](assets/screenshots/playing.png)

## 功能

- libmpv Render API 硬件加速播放
- 播放列表:拖入 / 添加多个文件、点击切歌、单曲移除,支持顺序播放 / 列表循环 / 单曲循环 / 随机播放,播完自动连播
- 记住音量、静音、倍速、循环模式、播放列表与窗口位置;再次打开同一文件时提示从上次位置继续
- 拖拽文件到窗口任意位置即可播放：列表为空时直接播放拖入的文件,已有列表时追加到列表尾部
- 无边框深色界面、Windows 11 圆角窗口与 macOS 风格窗口按钮
- 播放、暂停、进度拖动与前后 10 秒跳转,上一曲超过 3 秒先回到开头
- 进度条悬停缩略图预览,显示对应时间画面(按时间分桶缓存,异步生成)
- `-1` / `+1` 逐帧查看
- 音量调节、静音、倍速、音轨和字幕轨切换,音量最高可放大到 200%(滑条超过 100% 时变为琥珀色)
- 同名 LRC 歌词同步,支持 UTF-8、UTF-16、CP932 和 CP936
- 当前视频帧保存为 PNG
- 全屏、快捷键和自动隐藏控制栏

配置保存在 `%APPDATA%\neko-player\config.json`,删除该文件即可重置全部记忆状态。

## 快捷键

| 按键 | 功能 |
|---|---|
| `Space` | 播放 / 暂停 |
| `←` / `→` | 后退 / 前进 5 秒 |
| `↑` / `↓` | 调节音量 |
| `N` / `P` | 下一曲 / 上一曲 |
| `L` | 显示 / 隐藏播放列表 |
| `M` | 静音 |
| `F` | 全屏 |
| `Esc` | 退出全屏 |

## LRC 歌词

将歌词放在音乐文件旁并使用相同文件名：

```text
song.flac
song.lrc
```

## 运行环境

- Windows 10/11 x64
- 支持 OpenGL ES 3.x 的显卡驱动

下载 Release 压缩包后，保持 `neko-player.exe` 与 `libmpv-2.dll` 位于同一目录。

也可以运行 `neko-player-setup-1.1.1.exe` 安装。安装器支持常见音视频格式关联，会注册到 Windows「设置 → 默认应用」的候选列表，并会在资源管理器右键菜单中添加“通过 neko player 打开”。

## 从源码构建

需要 Rust MSVC 工具链和 libmpv 开发包。

1. 从 [zhongfly/mpv-winbuild](https://github.com/zhongfly/mpv-winbuild/releases) 下载 `mpv-dev-x86_64`。
2. 将头文件、DLL 和生成的 MSVC 导入库放入 `third_party/mpv/`：

   ```text
   third_party/mpv/include/mpv/*.h
   third_party/mpv/libmpv-2.dll
   third_party/mpv/libmpv-2.lib
   ```

3. 构建：

   ```powershell
   cargo build --release
   ```

4. 运行：

   ```powershell
   cargo run --release -- "path/to/video.mp4"
   ```

`build.rs` 会自动把 `libmpv-2.dll` 复制到输出目录。

## 构建安装包

安装 [Inno Setup 6](https://jrsoftware.org/isinfo.php)，完成 Release 构建后运行：

```powershell
iscc installer\neko-player.iss
```

安装包会生成在 `dist\` 目录。版本号取自脚本中的 `MyAppVersion` 默认值；GitHub Actions 构建时会自动改用 `Cargo.toml` 中的版本号。

## 持续集成

推送到 `main` 或提交 PR 时，[GitHub Actions](.github/workflows/build.yml) 会自动完成：下载 mpv 开发包、生成 MSVC 导入库、Release 构建、打包 ZIP、编译 Inno Setup 安装包。推送 `v*` tag 时会自动创建 GitHub Release 并上传两个产物。

若在仓库 secrets 中配置 `CERTIFICATE_BASE64`（ Authenticode 证书 PFX 的 base64）和 `CERTIFICATE_PASSWORD`，CI 会自动用 signtool 对主程序和安装包做代码签名；未配置时自动跳过。

## 技术架构

- Rust 2021
- Slint 1.17
- libmpv client/render API
- glow OpenGL ES
- Win32 无边框窗口集成

核心模块职责：

- `src/main.rs`：应用启动、OpenGL 渲染接线、UI 回调与生命周期编排
- `src/app_support.rs`：UI 同步、显示格式化、文件对话框与退出状态快照
- `src/player.rs`：libmpv 播放状态、命令、事件、轨道、歌词与播放列表
- `src/video_gl.rs`：libmpv Render API 与 Slint OpenGL 集成
- `src/settings.rs` / `src/thumb.rs` / `src/win32.rs`：配置、悬停缩略图与 Windows 原生集成

## 许可证

本项目基于 [MIT License](LICENSE) 开源。第三方依赖仍遵循其各自的许可证。
