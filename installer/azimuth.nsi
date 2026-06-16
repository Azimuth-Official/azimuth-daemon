; Azimuth Daemon NSIS Installer Script
; Packages daemon binary + NSSM service wrapper + config template

!include "MUI2.nsh"

; --- Metadata ---
Name "Azimuth Daemon"
OutFile "azimuth-daemon-setup.exe"
InstallDir "$PROGRAMFILES\Azimuth"
InstallDirRegKey HKLM "Software\Azimuth" "InstallDir"
RequestExecutionLevel admin

; --- Version info ---
VIProductVersion "1.0.0.0"
VIAddVersionKey "ProductName" "Azimuth Daemon"
VIAddVersionKey "CompanyName" "Azimuth"
VIAddVersionKey "FileDescription" "Azimuth SoOP positioning network - Tier 1 SDR node"
VIAddVersionKey "FileVersion" "1.0.0"
VIAddVersionKey "LegalCopyright" "Copyright (c) 2026 Azimuth"

; --- UI ---
!define MUI_ABORTWARNING
!define MUI_ICON "${NSISDIR}\Contrib\Graphics\Icons\modern-install.ico"
!define MUI_UNICON "${NSISDIR}\Contrib\Graphics\Icons\modern-uninstall.ico"

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "license.txt"
!insertmacro MUI_PAGE_DIRECTORY
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; --- Install ---
Section "Azimuth Daemon" SecDaemon
    SectionIn RO
    SetOutPath "$INSTDIR"

    ; Core files
    File "..\target\release\azimuth-daemon.exe"
    File "nssm.exe"
    File "..\config.example.toml"

    ; Create logs directory
    CreateDirectory "$INSTDIR\logs"

    ; Copy config template if no config exists yet
    IfFileExists "$INSTDIR\config.toml" skip_config
        CopyFiles /SILENT "$INSTDIR\config.example.toml" "$INSTDIR\config.toml"
    skip_config:

    ; Install NSSM service
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" install AzimuthDaemon "$INSTDIR\azimuth-daemon.exe"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppDirectory "$INSTDIR"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon DisplayName "Azimuth Daemon"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon Description "Azimuth SoOP positioning network - Tier 1 SDR node"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon Start SERVICE_AUTO_START'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppStdout "$INSTDIR\logs\daemon.log"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppStderr "$INSTDIR\logs\daemon-error.log"'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppRotateFiles 1'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppRotateBytes 10485760'

    ; Start Menu shortcuts
    CreateDirectory "$SMPROGRAMS\Azimuth"
    CreateShortCut "$SMPROGRAMS\Azimuth\Configure Azimuth.lnk" "$INSTDIR\azimuth-daemon.exe" "--setup"
    CreateShortCut "$SMPROGRAMS\Azimuth\Uninstall Azimuth.lnk" "$INSTDIR\uninstall.exe"

    ; Write uninstaller
    WriteUninstaller "$INSTDIR\uninstall.exe"

    ; Add/Remove Programs
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\AzimuthDaemon" \
        "DisplayName" "Azimuth Daemon"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\AzimuthDaemon" \
        "UninstallString" "$\"$INSTDIR\uninstall.exe$\""
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\AzimuthDaemon" \
        "Publisher" "Azimuth"
    WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\AzimuthDaemon" \
        "DisplayVersion" "1.0.0"
    WriteRegStr HKLM "Software\Azimuth" "InstallDir" "$INSTDIR"
SectionEnd

; --- Uninstall ---
Section "Uninstall"
    ; Stop and remove service
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" stop AzimuthDaemon'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" remove AzimuthDaemon confirm'

    ; Remove files (preserve config.toml)
    Delete "$INSTDIR\azimuth-daemon.exe"
    Delete "$INSTDIR\nssm.exe"
    Delete "$INSTDIR\config.example.toml"
    Delete "$INSTDIR\uninstall.exe"
    Delete "$INSTDIR\logs\daemon.log"
    Delete "$INSTDIR\logs\daemon-error.log"
    RMDir "$INSTDIR\logs"

    ; Remove Start Menu
    Delete "$SMPROGRAMS\Azimuth\Configure Azimuth.lnk"
    Delete "$SMPROGRAMS\Azimuth\Uninstall Azimuth.lnk"
    RMDir "$SMPROGRAMS\Azimuth"

    ; Remove registry (but not config.toml)
    DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\AzimuthDaemon"
    DeleteRegKey HKLM "Software\Azimuth"

    ; Remove install dir only if empty (config.toml preserved)
    RMDir "$INSTDIR"
SectionEnd
