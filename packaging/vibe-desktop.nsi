; Vibe Desktop installer - per-user (no admin/UAC), Start Menu entry,
; Add/Remove Programs registration. Built by release.yml via
;   makensis /DVERSION=<ver> packaging\vibe-desktop.nsi
; Relative paths in File/icon directives resolve against THIS file's dir.

!include "MUI2.nsh"

!ifndef VERSION
!define VERSION "0.0.0-dev"
!endif

Name "Vibe Desktop"
; Lands at repo root so the upload-artifact *.exe glob picks it up.
OutFile "..\vibe-desktop-${VERSION}-windows-x86_64-setup.exe"
InstallDir "$LOCALAPPDATA\Programs\Vibe Desktop"
InstallDirRegKey HKCU "Software\Vehicoule\Vibe Desktop" "InstallDir"
RequestExecutionLevel user
SetCompressor /SOLID lzma

!define MUI_ICON "icon.ico"
!define MUI_UNICON "icon.ico"

!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

Section "Install"
    SetOutPath "$INSTDIR"
    File "..\target\release\vibe-desktop.exe"

    CreateDirectory "$SMPROGRAMS\Vibe Desktop"
    CreateShortcut "$SMPROGRAMS\Vibe Desktop\Vibe Desktop.lnk" "$INSTDIR\vibe-desktop.exe"

    WriteUninstaller "$INSTDIR\Uninstall.exe"

    WriteRegStr HKCU "Software\Vehicoule\Vibe Desktop" "InstallDir" "$INSTDIR"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "DisplayName" "Vibe Desktop"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "UninstallString" '"$INSTDIR\Uninstall.exe"'
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "DisplayIcon" '"$INSTDIR\vibe-desktop.exe",0'
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "Publisher" "Vehicoule"
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "DisplayVersion" "${VERSION}"
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "NoModify" 1
    WriteRegDWORD HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop" "NoRepair" 1
SectionEnd

Section "Uninstall"
    Delete "$INSTDIR\vibe-desktop.exe"
    Delete "$INSTDIR\Uninstall.exe"
    RMDir "$INSTDIR"

    Delete "$SMPROGRAMS\Vibe Desktop\Vibe Desktop.lnk"
    RMDir "$SMPROGRAMS\Vibe Desktop"

    DeleteRegKey HKCU "Software\Microsoft\Windows\CurrentVersion\Uninstall\VibeDesktop"
    DeleteRegKey HKCU "Software\Vehicoule\Vibe Desktop"
SectionEnd
