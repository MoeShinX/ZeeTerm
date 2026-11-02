; ZeeTerm（Z-SSH 的 GPUI 原生版，试用）的独立 NSIS 安装程序：与 Tauri 版 Z-SSH 并存，不碰它的安装目录、卸载项和快捷方式。
; 只按当前用户安装，不需要管理员权限，也不安装 WebView2、Edge、Node 等共享运行时。
; 构建入口是 native/installer/build.ps1，构建期通过 /D 传入版本、暂存目录和输出路径。
;
; 命令行（与 Tauri 版安装器 / 更新器约定一致，更新器见 zssh-update 的 plan_install）：
;   /P        被动模式：只显示进度页，不弹向导页
;   /UPDATE   升级：沿用已有的安装目录；桌面快捷方式只在原来就有时才重建
;   /R        装完重新启动程序（仅被动 / 静默模式生效）
;   /ARGS ... 与 /R 配合：命令行里 /ARGS 之后的全部内容原样传给重启的程序
;   /S        NSIS 自带的静默安装
; 安装 / 卸载前会等程序（和 AI 工具桥）退出，不会强杀进程。

Unicode true
ManifestDPIAware true
RequestExecutionLevel user

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "LogicLib.nsh"
!include "FileFunc.nsh"
!include "WordFunc.nsh"

; ---------------- 构建期输入（由 build.ps1 通过 /D 传入） ----------------

!ifndef APP_VERSION
  !define APP_VERSION "0.0.0"
!endif

; NSIS 的版本资源要求四段数字，构建脚本会单独计算后传入。
!ifndef APP_VERSION_QUAD
  !define APP_VERSION_QUAD "${APP_VERSION}.0"
!endif

!ifndef STAGE_DIR
  !error "STAGE_DIR 未定义：请先运行 native/installer/build.ps1，或用 /DSTAGE_DIR=... 指定暂存目录。"
!endif

!ifndef OUT_FILE
  !define OUT_FILE "out\ZeeTerm-Setup-${APP_VERSION}.exe"
!endif

; ---------------- 应用信息 ----------------

!define APP_NAME "ZeeTerm"
!define APP_EXE "ZeeTerm.exe"
!define MCP_EXE "zssh-mcp.exe"
!define APP_ICON "icon.ico"
!define PUBLISHER "ZeeTerm"
!define UNINST_KEY "Software\Microsoft\Windows\CurrentVersion\Uninstall\ZeeTerm"

Name "${APP_NAME}"
BrandingText "${APP_NAME} ${APP_VERSION}"
OutFile "${OUT_FILE}"

; 按当前用户安装，避开 Program Files 和 UAC。
InstallDir "$LOCALAPPDATA\Programs\ZeeTerm"
; 重装时沿用上次选择的目录。
InstallDirRegKey HKCU "${UNINST_KEY}" "InstallLocation"

ShowInstDetails show
ShowUninstDetails show

SetCompressor /SOLID lzma
SetCompressorDictSize 64

VIProductVersion "${APP_VERSION_QUAD}"
VIAddVersionKey "ProductName" "${APP_NAME}"
VIAddVersionKey "FileDescription" "${APP_NAME} 安装程序"
VIAddVersionKey "FileVersion" "${APP_VERSION}"
VIAddVersionKey "ProductVersion" "${APP_VERSION}"
VIAddVersionKey "LegalCopyright" "GPL-3.0-or-later"

; ---------------- 界面 ----------------

!define MUI_ABORTWARNING
!define MUI_ICON "${STAGE_DIR}\${APP_ICON}"
!define MUI_UNICON "${STAGE_DIR}\${APP_ICON}"

!define MUI_WELCOMEPAGE_TITLE "欢迎安装 ${APP_NAME}"
!define MUI_WELCOMEPAGE_TEXT "ZeeTerm（Z-SSH 的 GPUI 原生版）目前是试用版本，可以和 Z-SSH（Tauri 版）同时安装。$\r$\n$\r$\n版本：${APP_VERSION}$\r$\n$\r$\n本安装程序只写入当前用户目录，不需要管理员权限；不会安装 WebView2、Edge、Node 或其他共享运行时。$\r$\n$\r$\n点击「下一步」选择安装目录。"

!define MUI_DIRECTORYPAGE_TEXT_TOP "请选择 ${APP_NAME} 的安装目录。安装程序只需要当前用户的写入权限，默认放在本地应用数据目录下。"
!define MUI_DIRECTORYPAGE_TEXT_DESTINATION "安装目录"

!define MUI_FINISHPAGE_TITLE "${APP_NAME} 安装完成"
!define MUI_FINISHPAGE_TEXT "${APP_NAME} ${APP_VERSION} 已安装到：$\r$\n$INSTDIR$\r$\n$\r$\n可以勾选下面的选项立即运行程序，也可以稍后从开始菜单启动。"
!define MUI_FINISHPAGE_RUN "$INSTDIR\${APP_EXE}"
!define MUI_FINISHPAGE_RUN_TEXT "立即运行 ${APP_NAME}"
!define MUI_FINISHPAGE_NOREBOOTSUPPORT

!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_WELCOME
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_DIRECTORY
Page custom OptionsPageCreate OptionsPageLeave
!insertmacro MUI_PAGE_INSTFILES
!define MUI_PAGE_CUSTOMFUNCTION_PRE SkipIfPassive
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

; 安装程序界面全部使用简体中文。
!insertmacro MUI_LANGUAGE "SimpChinese"

Var DesktopShortcutCheckbox
Var DesktopShortcutState
Var PassiveMode
Var UpdateMode

; ---------------- 安装 ----------------

Section "安装 ${APP_NAME}" SEC_MAIN
  SectionIn RO
  SetShellVarContext current

  ; 旧进程还在跑时覆盖 exe 会弹「写入失败」：先等它退出。
  Call WaitForAppExit

  SetOutPath "$INSTDIR"
  File /oname=${APP_EXE} "${STAGE_DIR}\${APP_EXE}"
  ; AI 工具桥要求 zssh-mcp.exe 和主程序在同一目录。
  File /oname=${MCP_EXE} "${STAGE_DIR}\${MCP_EXE}"
  File "${STAGE_DIR}\LICENSE"
  ; NOTICE 是根 NOTICE 与 native/NOTICE 合并后的文件（build.ps1 生成），不是仓库里的单个原文件。
  File "${STAGE_DIR}\NOTICE"
  File "${STAGE_DIR}\${APP_ICON}"

  WriteUninstaller "$INSTDIR\uninstall.exe"

  ; 开始菜单快捷方式始终创建；桌面快捷方式由安装选项页决定。
  CreateDirectory "$SMPROGRAMS"
  CreateShortCut "$SMPROGRAMS\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_ICON}" 0 SW_SHOWNORMAL "" "${APP_NAME}"

  ${If} $DesktopShortcutState == ${BST_CHECKED}
    CreateShortCut "$DESKTOP\${APP_NAME}.lnk" "$INSTDIR\${APP_EXE}" "" "$INSTDIR\${APP_ICON}" 0 SW_SHOWNORMAL "" "${APP_NAME}"
  ${EndIf}

  ; 当前用户的卸载信息，显示在「应用和功能」里。
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayName" "${APP_NAME}"
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayVersion" "${APP_VERSION}"
  WriteRegStr HKCU "${UNINST_KEY}" "Publisher" "${PUBLISHER}"
  WriteRegStr HKCU "${UNINST_KEY}" "InstallLocation" "$INSTDIR"
  WriteRegStr HKCU "${UNINST_KEY}" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKCU "${UNINST_KEY}" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKCU "${UNINST_KEY}" "DisplayIcon" '"$INSTDIR\${APP_EXE}",0'
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoModify" 1
  WriteRegDWORD HKCU "${UNINST_KEY}" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKCU "${UNINST_KEY}" "EstimatedSize" "$0"

  ; 被动安装和升级装完自动关闭进度页。
  ${If} $PassiveMode = 1
  ${OrIf} $UpdateMode = 1
    SetAutoClose true
  ${EndIf}
SectionEnd

; ---------------- 卸载 ----------------

Section "Uninstall"
  SetShellVarContext current

  Call un.WaitForAppExit
  ; 只删程序自己的文件、快捷方式和卸载项；不删用户数据（%APPDATA% 下的 ZeeTerm 目录：主机库、设置、指纹）。
  ; Tauri 版的卸载默认同样保留数据，它另有一个「同时删除应用数据」的可选项，这里没有。
  ; RMDir 不带 /r：安装目录里有用户自己放的东西时整个目录保留。

  Delete "$SMPROGRAMS\${APP_NAME}.lnk"
  Delete "$DESKTOP\${APP_NAME}.lnk"

  Delete "$INSTDIR\${APP_EXE}"
  Delete "$INSTDIR\${MCP_EXE}"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\NOTICE"
  Delete "$INSTDIR\${APP_ICON}"
  Delete "$INSTDIR\uninstall.exe"
  RMDir "$INSTDIR"

  DeleteRegKey HKCU "${UNINST_KEY}"
SectionEnd

; ---------------- 回调 ----------------

Function .onInit
  ; 命令行开关：只看有没有，不关心取值（与 Tauri 版一致）。
  ${GetOptions} $CMDLINE "/P" $R0
  ${IfNot} ${Errors}
    StrCpy $PassiveMode 1
  ${EndIf}
  ${GetOptions} $CMDLINE "/UPDATE" $R0
  ${IfNot} ${Errors}
    StrCpy $UpdateMode 1
  ${EndIf}

  SetShellVarContext current
  ; 桌面快捷方式默认勾选；升级时尊重用户当初的选择：原来没有就不再建。
  StrCpy $DesktopShortcutState ${BST_CHECKED}
  ${If} $UpdateMode = 1
  ${AndIfNot} ${FileExists} "$DESKTOP\${APP_NAME}.lnk"
    StrCpy $DesktopShortcutState ${BST_UNCHECKED}
  ${EndIf}
FunctionEnd

; 被动模式不显示向导页。
Function SkipIfPassive
  ${If} $PassiveMode = 1
    Abort
  ${EndIf}
FunctionEnd

; 装完按 /R 重启程序。图形安装向导里由完成页的「立即运行」负责，所以只在被动 / 静默时处理。
Function .onInstSuccess
  ${If} $PassiveMode = 1
  ${OrIf} ${Silent}
    ${GetOptions} $CMDLINE "/R" $R0
    ${IfNot} ${Errors}
      ; /ARGS 之后到结尾都是要传回去的参数；没有 /ARGS 就不带参数。
      StrCpy $R1 ""
      ClearErrors
      ${WordFind} "$CMDLINE" "/ARGS " "E+1}" $R1
      ${If} ${Errors}
        StrCpy $R1 ""
      ${EndIf}
      Exec '"$INSTDIR\${APP_EXE}" $R1'
    ${EndIf}
  ${EndIf}
FunctionEnd

; 判断一个文件能不能以写方式打开：正在运行的 exe 会拒绝，据此知道旧进程还没退出。
!macro ZSSH_FILE_BUSY PATH RESULTVAR
  ${If} ${FileExists} "${PATH}"
    ClearErrors
    FileOpen $R1 "${PATH}" a
    ${If} ${Errors}
      StrCpy ${RESULTVAR} 1
    ${Else}
      FileClose $R1
      StrCpy ${RESULTVAR} 0
    ${EndIf}
  ${Else}
    StrCpy ${RESULTVAR} 0
  ${EndIf}
!macroend

; 等主程序和 AI 工具桥退出（最多约 15 秒，更新器启动安装器后自己才开始退出）。
; 超时：图形 / 被动模式让用户选「重试 / 取消」；静默模式直接失败（退出码 5）。
; 不强杀进程：外部智能体可能正在用 zssh-mcp.exe，用户应该自己决定。
!macro ZSSH_WAIT_FN PFX
Function ${PFX}WaitForAppExit
  Push $R0
  Push $R1
  Push $R2
  Push $R3
  zssh_start:
  StrCpy $R3 0
  zssh_check:
  !insertmacro ZSSH_FILE_BUSY "$INSTDIR\${APP_EXE}" $R0
  !insertmacro ZSSH_FILE_BUSY "$INSTDIR\${MCP_EXE}" $R2
  ${If} $R0 == 0
  ${AndIf} $R2 == 0
    Goto zssh_done
  ${EndIf}
  ${If} $R3 < 30
    IntOp $R3 $R3 + 1
    Sleep 500
    Goto zssh_check
  ${EndIf}
  MessageBox MB_RETRYCANCEL|MB_ICONEXCLAMATION "${APP_NAME} 或它的 AI 工具桥仍在运行，无法继续。$\r$\n$\r$\n请先退出它们（包括托盘图标和正在使用 AI 工具的智能体），再点「重试」。" /SD IDCANCEL IDRETRY zssh_start
  SetErrorLevel 5
  Abort "程序仍在运行，已取消。"
  zssh_done:
  Pop $R3
  Pop $R2
  Pop $R1
  Pop $R0
FunctionEnd
!macroend
!insertmacro ZSSH_WAIT_FN ""
!insertmacro ZSSH_WAIT_FN "un."

Function un.onInit
  SetShellVarContext current
FunctionEnd

Function OptionsPageCreate
  !insertmacro MUI_HEADER_TEXT "安装选项" "确认版本、安装目录和快捷方式"

  nsDialogs::Create 1018
  Pop $0
  ${If} $0 == error
    Abort
  ${EndIf}

  ${NSD_CreateLabel} 0 0 100% 14u "版本：${APP_VERSION}"
  Pop $1
  ${NSD_CreateLabel} 0 18u 100% 14u "安装目录：$INSTDIR"
  Pop $1
  ${NSD_CreateLabel} 0 42u 100% 34u "安装程序只写入当前用户目录，不需要管理员权限；不会安装 WebView2、Edge、Node 或其他共享运行时。"
  Pop $1
  ${NSD_CreateCheckbox} 0 88u 100% 12u "在桌面创建快捷方式"
  Pop $DesktopShortcutCheckbox
  ${NSD_SetState} $DesktopShortcutCheckbox $DesktopShortcutState

  nsDialogs::Show
FunctionEnd

Function OptionsPageLeave
  ${NSD_GetState} $DesktopShortcutCheckbox $DesktopShortcutState
FunctionEnd
