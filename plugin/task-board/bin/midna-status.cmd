@echo off
rem Midna status-bar item on Windows, as midna-status is elsewhere: the terminal's task-board goal,
rem task and line. Midna runs an item's path itself, which can't be a sh script on Windows. The app's
rem copy is in its install folder's plugin\task-board\bin, three folders below tb.exe.
set "tb=%~dp0..\..\..\tb.exe"
if not exist "%tb%" set "tb=%USERPROFILE%\.local\bin\tb.exe"
"%tb%" peek --midna 2>nul || echo []
