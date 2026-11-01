; WrongIndex installer hooks (WI-701), included by Tauri's NSIS template.
;
; The service (wrongindexd.exe) keeps running after the desktop window is
; closed, so its files would be locked during an upgrade or an uninstall.
; Both stop it first through the installed CLI; the data root and every
; project index stay where they are. An upgrade restarts the service
; afterwards if it was running before.

Var WiServiceWasRunning

!macro WRONGINDEX_STOP_SERVICE
  ${If} ${FileExists} "$INSTDIR\wrongindex.exe"
    nsExec::Exec '"$INSTDIR\wrongindex.exe" daemon stop --wait-seconds 30'
    Pop $0 ; exit code; a service that is not running is not an error
    ; The discovery record goes before the process exits; give it a moment
    ; to release its executable.
    Sleep 1500
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREINSTALL
  StrCpy $WiServiceWasRunning "0"
  ${If} ${FileExists} "$INSTDIR\wrongindex.exe"
    ; `daemon status` exits 0 only while the service answers.
    nsExec::Exec '"$INSTDIR\wrongindex.exe" daemon status'
    Pop $0
    ${If} $0 == "0"
      StrCpy $WiServiceWasRunning "1"
    ${EndIf}
  ${EndIf}
  !insertmacro WRONGINDEX_STOP_SERVICE
!macroend

!macro NSIS_HOOK_POSTINSTALL
  ${If} $WiServiceWasRunning == "1"
    ; Starts the new service without opening a window, then exits.
    nsExec::Exec '"$INSTDIR\wrongindex-desktop.exe" --start-service'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro WRONGINDEX_STOP_SERVICE
  ; "Start the index service when I sign in" (Settings) points at this copy.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "WrongIndex"
!macroend
