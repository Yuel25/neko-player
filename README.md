# neko player

一款使用 Rust、Slint 和 libmpv 构建的现代 Windows 媒体播放器。

![neko player icon](assets/neko-player-icon.png)

## 界面预览

### 初始界面

![neko player 初始界面](assets/screenshots/home.png)

### 视频播放

![neko player 视频播放界面](assets/screenshots/playing.png)

## 功能

- libmpv Render API 硬件加速播放
- 无边框深色界面与 macOS 风格窗口按钮
- 播放、暂停、进度拖动与前后 10 秒跳转
- `-1` / `+1` 逐帧查看
- 音量、静音、倍速、音轨和字幕轨切换
- 同名 LRC 歌词同步，支持 UTF-8、UTF-16、CP932 和 CP936
- 当前视频帧保存为 PNG
- 全屏、快捷键和自动隐藏控制栏

## 快捷键

| 按键 | 功能 |
|---|---|
| `Space` | 播放 / 暂停 |
| `←` / `→` | 后退 / 前进 5 秒 |
| `↑` / `↓` | 调节音量 |
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

也可以运行 `neko-player-setup-0.1.0.exe` 安装。安装器支持常见音视频格式关联，并会在资源管理器右键菜单中添加“通过 neko player 打开”。

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

安装包会生成在 `dist\` 目录。

## 技术架构

- Rust 2021
- Slint 1.17
- libmpv client/render API
- glow OpenGL ES
- Win32 无边框窗口集成
