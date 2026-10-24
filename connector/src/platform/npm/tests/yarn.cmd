@IF EXIST "%~dp0\node.exe" (
  "%~dp0\node.exe"  "%~dp0\..\Data\global\node_modules\@example\agent\bin\agent.js" %*
) ELSE (
  @SETLOCAL
  @SET PATHEXT=%PATHEXT:;.JS;=;%
  node  "%~dp0\..\Data\global\node_modules\@example\agent\bin\agent.js" %*
)