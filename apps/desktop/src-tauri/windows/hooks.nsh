; Callimachus NSIS installer hooks (bundle.windows.nsis.installerHooks in tauri.conf.json).

!macro NSIS_HOOK_PREUNINSTALL
  ; Before the files go: remove the agent integrations (the /recall skill, MCP entries, the
  ; Claude Code hooks and the cal.exe link). On Windows the hooks are plain commands with no
  ; missing-binary guard, so leaving them would make every new Claude Code session report a
  ; hook error after an uninstall. cal.exe would also keep $INSTDIR from being removed.
  ;
  ; In-app updates re-run the uninstaller in update mode (/UPDATE); keep the integration then.
  ; A manually run newer (or older) setup.exe is NOT update mode: its default "uninstall
  ; before installing" choice runs this uninstaller plainly, so the removal happens then too.
  ; So does an uninstall the user cancels at the app-is-running prompt, which comes after
  ; this hook. That is why --uninstall-integration records what it removed in a marker in
  ; the app data dir, which an uninstall leaves in place: the next launch of the app
  ; restores those pieces and deletes the marker.
  ;
  ; Known gap: the .msi (bundle.targets is "all") has no equivalent step, since Tauri only
  ; takes installer hooks for NSIS. Uninstalling an MSI install leaves the hooks pointing at
  ; the deleted exe until the user runs `callimachus.exe --uninstall-integration` first or
  ; removes them in Settings. Fixing that needs a WiX custom action (bundle.windows.wix
  ; fragmentPaths) tested on a Windows machine, or a missing-binary guard in the hooks.
  ${If} $UpdateMode <> 1
    ExecWait '"$INSTDIR\${MAINBINARYNAME}.exe" --uninstall-integration'
    Delete "$INSTDIR\cal.exe"
  ${EndIf}
!macroend
