@IF EXIST "%~dp0\node.exe" (
  "%~dp0\node.exe"  "%~dp0\..\Data\global\node_modules\@openai\codex\bin\codex.js" %*
) ELSE (
  @SETLOCAL
  @SET PATHEXT=%PATHEXT:;.JS;=;%
  node  "%~dp0\..\Data\global\node_modules\@openai\codex\bin\codex.js" %*
)