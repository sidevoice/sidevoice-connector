@SETLOCAL

@IF EXIST "%~dp0\node.exe" (
  @SET "_prog=%~dp0\node.exe"
) ELSE (
  @SET "_prog=node"
  @SET PATHEXT=%PATHEXT:;.JS;=;%
)

"%_prog%"  "%~dp0\..\Data\global\node_modules\@example\agent\bin\agent.js" %*
@ENDLOCAL
