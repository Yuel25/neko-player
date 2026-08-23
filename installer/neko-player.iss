#define MyAppName "neko player"
; CI 会用 /DMyAppVersion=<Cargo.toml 版本> 覆盖；本地手动编译时使用此默认值
#ifndef MyAppVersion
#define MyAppVersion "1.0.0"
#endif
#define MyAppExeName "neko-player.exe"

[Setup]
AppId={{F2B5E5AC-1C41-4C57-975C-99BA590314EF}
AppName={#MyAppName}
AppVersion={#MyAppVersion}
AppPublisher=Yuel25
AppPublisherURL=https://github.com/Yuel25/neko-player
AppSupportURL=https://github.com/Yuel25/neko-player/issues
DefaultDirName={localappdata}\Programs\Neko Player
DefaultGroupName={#MyAppName}
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
OutputDir=..\dist
OutputBaseFilename=neko-player-setup-{#MyAppVersion}
SetupIconFile=..\assets\neko-player.ico
UninstallDisplayIcon={app}\{#MyAppExeName}
Compression=lzma2/ultra64
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
ChangesAssociations=yes


[Tasks]
Name: "desktopicon"; Description: "创建桌面快捷方式"; GroupDescription: "快捷方式："; Flags: unchecked
Name: "associate"; Description: "关联常见音视频格式"; GroupDescription: "文件关联："; Flags: unchecked

[Files]
Source: "..\target\release\neko-player.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\target\release\libmpv-2.dll"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{autoprograms}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"
Name: "{autodesktop}\{#MyAppName}"; Filename: "{app}\{#MyAppExeName}"; Tasks: desktopicon

[Registry]
Root: HKCU; Subkey: "Software\Classes\Applications\{#MyAppExeName}"; ValueType: string; ValueName: "FriendlyAppName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\Applications\{#MyAppExeName}\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\NekoPlayer.Media"; ValueType: string; ValueData: "neko player 媒体文件"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\NekoPlayer.Media\DefaultIcon"; ValueType: string; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\Classes\NekoPlayer.Media\shell\open\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
; Default Programs Capabilities：让 neko player 出现在
; Windows「设置 → 应用 → 默认应用」的候选列表中（各扩展名的映射在 extension.iss 中注册）
Root: HKCU; Subkey: "Software\Neko Player\Capabilities"; ValueType: string; ValueName: "ApplicationName"; ValueData: "{#MyAppName}"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Neko Player\Capabilities"; ValueType: string; ValueName: "ApplicationDescription"; ValueData: "基于 libmpv 与 Slint 的现代 Windows 媒体播放器"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\RegisteredApplications"; ValueType: string; ValueName: "neko player"; ValueData: "Software\Neko Player\Capabilities"; Flags: uninsdeletevalue
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\video\shell\NekoPlayer"; ValueType: string; ValueData: "通过 neko player 打开"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\video\shell\NekoPlayer"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\video\shell\NekoPlayer\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\audio\shell\NekoPlayer"; ValueType: string; ValueData: "通过 neko player 打开"; Flags: uninsdeletekey
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\audio\shell\NekoPlayer"; ValueType: string; ValueName: "Icon"; ValueData: "{app}\{#MyAppExeName},0"
Root: HKCU; Subkey: "Software\Classes\SystemFileAssociations\audio\shell\NekoPlayer\command"; ValueType: string; ValueData: """{app}\{#MyAppExeName}"" ""%1"""

#define Ext "mp4"
#include "extension.iss"
#define Ext "mkv"
#include "extension.iss"
#define Ext "webm"
#include "extension.iss"
#define Ext "avi"
#include "extension.iss"
#define Ext "mov"
#include "extension.iss"
#define Ext "flv"
#include "extension.iss"
#define Ext "ts"
#include "extension.iss"
#define Ext "m2ts"
#include "extension.iss"
#define Ext "mp3"
#include "extension.iss"
#define Ext "flac"
#include "extension.iss"
#define Ext "ogg"
#include "extension.iss"
#define Ext "wav"
#include "extension.iss"
#define Ext "opus"
#include "extension.iss"
#define Ext "m4a"
#include "extension.iss"

[Run]
Filename: "{app}\{#MyAppExeName}"; Description: "启动 {#MyAppName}"; Flags: nowait postinstall skipifsilent
