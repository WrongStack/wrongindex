; WrongIndex installer hooks (WI-701), included by Tauri's NSIS template.
;
; The service (wrongindexd.exe) keeps running after the WrongIndex window
; is closed, so its files would be locked during an upgrade or an uninstall.
; Both stop it first through the installed CLI (gracefully, then whatever
; still runs from the install directory); the data root and every project
; index stay where they are. An upgrade restarts the service
; afterwards if it was running before.
;
; The install directory goes on the user's PATH (so `wrongindex` works in a
; new terminal) and comes off it on uninstall; the CLI edits the value and
; keeps every other entry as written.
;
; Up to 0.2.0 the window was a Tauri program, wrongindex-desktop.exe, with a
; separate "WrongIndex Native (Preview)" shortcut for this one. An upgrade
; removes both (the template also deletes the previous main binary and
; points the WrongIndex shortcuts at the new one) and moves the sign-in
; entry over when it pointed at this copy's old program.

Var WiServiceWasRunning

!macro WRONGINDEX_STOP_SERVICE
  ${If} ${FileExists} "$INSTDIR\wrongindex.exe"
    nsExec::Exec '"$INSTDIR\wrongindex.exe" daemon stop --wait-seconds 30'
    Pop $0 ; exit code; a service that is not running is not an error
    ; `daemon stop` reaches only the service of the data root this user's
    ; environment names. One started with another (`--data-root`) would keep
    ; its files locked: end whatever still runs from here, except what
    ; started this. (A CLI older than 0.1.2 lacks the command; harmless.)
    nsExec::Exec '"$INSTDIR\wrongindex.exe" setup stop-installed "$INSTDIR"'
    Pop $0
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

!macro WRONGINDEX_REMOVE_RETIRED
  Delete "$SMPROGRAMS\WrongIndex Native (Preview).lnk"
  Delete "$INSTDIR\wrongindex-desktop.exe"
!macroend

!macro NSIS_HOOK_POSTINSTALL
  !insertmacro WRONGINDEX_REMOVE_RETIRED
  ; "Start the index service when I sign in" set by the retired program:
  ; the same entry, now through this one (still no window at sign-in).
  ReadRegStr $0 HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "WrongIndex"
  ${If} $0 == '"$INSTDIR\wrongindex-desktop.exe" --start-service'
    WriteRegStr HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "WrongIndex" '"$INSTDIR\wrongindex-gpui.exe" --start-service'
  ${EndIf}
  nsExec::Exec '"$INSTDIR\wrongindex.exe" setup add-to-path "$INSTDIR"'
  Pop $0
  ${If} $WiServiceWasRunning == "1"
    ; Starts the new service without opening a window, then exits.
    nsExec::Exec '"$INSTDIR\wrongindex-gpui.exe" --start-service'
    Pop $0
  ${EndIf}
!macroend

!macro NSIS_HOOK_PREUNINSTALL
  !insertmacro WRONGINDEX_REMOVE_RETIRED
  !insertmacro WRONGINDEX_STOP_SERVICE
  nsExec::Exec '"$INSTDIR\wrongindex.exe" setup remove-from-path "$INSTDIR"'
  Pop $0
  ; Left by `wrongindex self-update` (a running CLI cannot delete itself).
  Delete "$INSTDIR\wrongindex.exe.old"
  ; "Start the index service when I sign in" (Settings) points at this copy.
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "WrongIndex"
!macroend
