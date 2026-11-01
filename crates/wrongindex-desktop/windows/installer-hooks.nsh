; WrongIndex installer hooks (WI-701), included by Tauri's NSIS template.
;
; The service (wrongindexd.exe) keeps running after the desktop window is
; closed, so its files would be locked during an upgrade or an uninstall.
; Both stop it first through the installed CLI; the data root and every
; project index stay where they are.

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
  !insertmacro WRONGINDEX_STOP_SERVICE
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro WRONGINDEX_STOP_SERVICE
  ; "Start the index service when I sign in" (Settings) points at this copy.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "WrongIndex"
!macroend
