# SayTTS

把你对着麦克风说的话转成文字，再用选好的声音念出来，送到直播或语音软件里。同时提供一个可以加进 OBS 的字幕页。

转写和朗读由 [Fish Audio](https://fish.audio) 提供，需要你自己的 API key。

## 下载安装

在 [Releases](../../releases) 下载对应系统的文件：

| 系统 | 文件 |
| --- | --- |
| macOS（Apple 芯片） | `*_macos_arm64.dmg` |
| macOS（Intel） | `*_macos_x64.dmg` |
| Windows | `*_windows_x64-setup.exe`，或免安装的 `*_portable.exe` |
| Linux | `*.AppImage`、`*.deb` 或 `*.rpm` |

安装包没有签名，第一次打开可能被系统拦下：

- macOS 提示“已损坏”或“无法验证开发者”：在终端执行 `xattr -dr com.apple.quarantine /Applications/SayTTS.app`
- Windows 弹出 SmartScreen：点“更多信息”→“仍要运行”

## 使用

1. 第一次打开时填入 Fish Audio 的 API key。
2. 在设置里选择麦克风和直播输出设备。
3. 选一个声音，点开始，然后说话。

想让直播软件或语音软件收到朗读的声音，需要一个虚拟声卡：macOS 可用 [BlackHole](https://github.com/ExistentialAudio/BlackHole)，Windows 可用 [VB-CABLE](https://vb-audio.com/Cable/)。把“直播输出”设为虚拟声卡，再在直播或语音软件里把它选作麦克风。

不说话时也可以在底部输入框打字，回车朗读。

### OBS 字幕

在设置的“字幕页”里复制地址，在 OBS 里添加“浏览器”来源并粘贴这个地址。
