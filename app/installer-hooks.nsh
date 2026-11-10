; DriveCraft installer hooks.
;
; Before its files are removed, DriveCraft is asked to put back what it changed on
; this computer outside its own folders: the browser's Sync button, the folders it
; listed in File Explorer and their right-click menu, and starting with Windows.
; The user's files, the catalog, the sign-in and the logs are left alone.
!macro NSIS_HOOK_PREUNINSTALL
  DetailPrint "Giving back the Sync button and File Explorer..."
  ExecWait '"$INSTDIR\drivecraft-app.exe" --uninstall-cleanup'
!macroend
