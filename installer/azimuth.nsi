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

    ; LGPL-3.0 (libwdi, linked into the daemon for driver installation) and the
    ; third-party notices must travel with the binary.
    SetOutPath "$INSTDIR\licenses"
    File "..\licenses\LGPL-3.0.txt"
    File "..\licenses\LGPL-2.1.txt"
    File "..\LICENSE"
    File "..\NOTICE"
    File "..\THIRD_PARTY_NOTICES.md"
    SetOutPath "$INSTDIR"

    ; Create logs directory
    CreateDirectory "$INSTDIR\logs"

    ; Copy config template if no config exists yet
    IfFileExists "$INSTDIR\config.toml" skip_config
        CopyFiles /SILENT "$INSTDIR\config.example.toml" "$INSTDIR\config.toml"
    skip_config:

    ; Machine-scoped identity dir: %ProgramData%\Azimuth holds node-identity.json
    ; so the node keeps its identity across uninstall/reinstall. Grant Users
    ; Modify so the user-context desktop sidecar writes the SAME file as the
    ; SYSTEM-context service (one identity per machine -- no split-brain).
    ReadEnvStr $0 "ProgramData"
    CreateDirectory "$0\Azimuth"
    nsExec::ExecToLog 'icacls "$0\Azimuth" /grant "*S-1-5-32-545:(OI)(CI)M"'

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
    ; Exit-code policy: 0 = stopped on request; 43 = another Azimuth daemon holds
    ; the status port (do NOT restart, or the pair flaps forever); anything else
    ; restarts.
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppExit Default Restart'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppExit 0 Exit'
    nsExec::ExecToLog '"$INSTDIR\nssm.exe" set AzimuthDaemon AppExit 43 Exit'

    ; Bind WinUSB to Interface 0 of any RTL-SDR already attached. The installer
    ; is elevated, so no extra prompt. Non-fatal: the app offers "Install SDR
    ; driver" later if this finds no device or fails.
    nsExec::ExecToLog '"$INSTDIR\azimuth-daemon.exe" --install-driver'
    Pop $1

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
    Delete "$INSTDIR\licenses\LGPL-3.0.txt"
    Delete "$INSTDIR\licenses\LGPL-2.1.txt"
    Delete "$INSTDIR\licenses\LICENSE"
    Delete "$INSTDIR\licenses\NOTICE"
    Delete "$INSTDIR\licenses\THIRD_PARTY_NOTICES.md"
    RMDir "$INSTDIR\licenses"
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

    ; INVARIANT: never delete %ProgramData%\Azimuth or node-identity.json.
    ; That file is what prevents duplicate node registrations after reinstall.
SectionEnd
