# SayTTS

把麦克风里说的话实时转成文字，再用选定的声音朗读到指定的输出设备，同时提供 OBS 字幕页。转写和朗读使用 [Fish Audio](https://fish.audio)，需要自己的 API key。

## 下载

在 [Releases](../../releases) 里下载对应平台的安装包：

| 平台 | 文件 |
| --- | --- |
| macOS（Apple 芯片） | `*_macos_arm64.dmg` |
| macOS（Intel） | `*_macos_x64.dmg` |
| Windows | `*_windows_x64-setup.exe`、`*.msi`，或免安装的 `*_portable.exe` |
| Linux | `*.AppImage`、`*.deb`、`*.rpm` |

安装包没有签名：

- macOS 提示“已损坏”或“无法验证开发者”时，执行 `xattr -dr com.apple.quarantine /Applications/SayTTS.app`。
- Windows SmartScreen 拦截时，点“更多信息 → 仍要运行”。

## 从源码构建

需要 Rust 和 Node.js（用于 Tauri CLI）。

```bash
cd voice-relay/app
npx @tauri-apps/cli@2 build
```

开发时直接运行：

```bash
cd voice-relay
cargo run -p voice-relay
```

## 发布

推送 `v*` 标签后，GitHub Actions 会在 macOS、Windows、Linux 上构建并创建 Release。
