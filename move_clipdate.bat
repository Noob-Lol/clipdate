@echo off
setlocal
: check choco var
if not defined ChocolateyInstall (echo "Chocolatey not installed" && goto end)

set "script_path=%~dp0"
set "INSTALL_PATH=%ChocolateyInstall%\bin\"

: its rust
set "RELEASE_PATH=%script_path%\target\release\"
set "BINARY_NAME=clipdate.exe"

if exist "%RELEASE_PATH%" (
    move /y "%RELEASE_PATH%%BINARY_NAME%" "%INSTALL_PATH%"
) else (
    echo "release binary not found"
    goto end
)


:end
if /i "%comspec% /c %~0 " equ "%cmdcmdline:"=%" timeout /t 3 /nobreak
endlocal
exit /b
