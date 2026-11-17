@echo off
rem start-v86.cmd: double-click to start everything for v86.
rem   1. installs the app's packages the first time (npm, as the retro + bun README says)
rem   2. starts the v86 server (Rust) in its own window, if it has been built
rem   3. starts retro + bun (Figma sync) and Expo, targeting v86-app, in its own window
rem Then open the retro + bun plugin in Figma and click "Sync to Expo".

setlocal
set "ROOT=%~dp0"
set "APP=%ROOT%v86-app"
set "SERVER_EXE=%ROOT%v86-server\gates\gm_rest\target\release\v86_server.exe"
set "SYNC=C:\Users\shepherd\Videos\food\figma\retro + bun\for-expo\mixes-server.ts"

if not exist "%APP%\package.json" (
  echo Cannot find %APP%\package.json
  pause
  exit /b 1
)
if not exist "%SYNC%" (
  echo Cannot find the retro + bun sync server:
  echo   %SYNC%
  pause
  exit /b 1
)

if not exist "%APP%\node_modules\.bin\expo.cmd" (
  echo First run: installing v86-app packages. This takes a few minutes...
  pushd "%APP%"
  call npm install --no-audit --no-fund
  if errorlevel 1 (
    echo npm install failed.
    popd
    pause
    exit /b 1
  )
  popd
)

if exist "%SERVER_EXE%" (
  echo Starting the v86 server on port 4000...
  start "v86 server (port 4000)" /d "%ROOT%v86-server\gates\gm_rest" cmd /k "set PORT=4000&& target\release\v86_server.exe"
) else (
  echo v86 server not built yet. To build it: cd v86-server\gates\gm_rest ^&^& cargo build --release
)

echo Starting retro + bun and Expo for v86-app...
start "v86-app: retro + bun + Expo" /d "%APP%" cmd /k bun "%SYNC%" --expo

echo.
echo Both windows are open. Close them to stop.
timeout /t 5 >nul
endlocal
