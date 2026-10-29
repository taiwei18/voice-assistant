# Current-user installation; rerun to update. Settings in APPDATA are preserved.
param([string]$Source = (Join-Path $PSScriptRoot 'dist'), [switch]$Launch)
$ErrorActionPreference = 'Stop'
$installRoot = [IO.Path]::GetFullPath((Join-Path $env:LOCALAPPDATA 'Programs\VoiceAssistant'))
$sourceRoot = [IO.Path]::GetFullPath($Source)
$marker = Join-Path $installRoot 'installation.json'
if ($sourceRoot -eq $installRoot) { throw '安装来源不能是安装目录本身。' }
$sourceExe = Join-Path $sourceRoot 'voa.exe'
if (!(Test-Path -LiteralPath $sourceExe) -or !(Test-Path -LiteralPath (Join-Path $sourceRoot 'models\kws\tokens.txt'))) { throw '发布文件不完整，请先编译发布版。' }
if ((Test-Path -LiteralPath $installRoot) -and !(Test-Path -LiteralPath $marker)) { throw '安装目录已存在但不是本程序管理的安装，停止覆盖。' }
$stage = Join-Path $env:TEMP ('VoiceAssistant-install-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $stage | Out-Null
try {
    Copy-Item -LiteralPath $sourceExe -Destination $stage
    Get-ChildItem -LiteralPath $sourceRoot -Filter '*.dll' | Copy-Item -Destination $stage
    Copy-Item -LiteralPath (Join-Path $sourceRoot 'models') -Destination $stage -Recurse
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'uninstall.ps1') -Destination $stage
    if ((Get-FileHash -LiteralPath $sourceExe).Hash -ne (Get-FileHash -LiteralPath (Join-Path $stage 'voa.exe')).Hash) { throw '安装文件校验失败。' }
    $exe = Join-Path $installRoot 'voa.exe'
    $portableExe = Join-Path $sourceRoot 'voa.exe'
    # 改名前叫 voice-assistant.exe，快捷方式叫“语音助手”
    $oldExe = Join-Path $installRoot 'voice-assistant.exe'
    $running = @(Get-Process voa, voice-assistant -ErrorAction SilentlyContinue | Where-Object { $_.Path -in @($exe, $portableExe, $oldExe, (Join-Path $sourceRoot 'voice-assistant.exe')) })
    $wasRunning = $running.Count -gt 0
    $backup = Join-Path $stage 'backup'
    if (Test-Path -LiteralPath $marker) {
        New-Item -ItemType Directory -Path $backup | Out-Null
        Get-ChildItem -LiteralPath $installRoot | Copy-Item -Destination $backup -Recurse
    }
    foreach ($process in $running) { Stop-Process -Id $process.Id -Force; $process.WaitForExit(10000) | Out-Null }
    New-Item -ItemType Directory -Path $installRoot -Force | Out-Null
    try {
        Get-ChildItem -LiteralPath $stage | Where-Object Name -ne 'backup' | Copy-Item -Destination $installRoot -Recurse -Force
        Remove-Item -LiteralPath $oldExe -ErrorAction SilentlyContinue
        @{ installedAt = (Get-Date).ToString('o'); source = $sourceRoot; version = '0.1.0'; sha256 = (Get-FileHash -LiteralPath $exe).Hash } | ConvertTo-Json | Set-Content -LiteralPath $marker -Encoding UTF8
    } catch {
        if (Test-Path -LiteralPath $backup) { Get-ChildItem -LiteralPath $backup | Copy-Item -Destination $installRoot -Recurse -Force }
        if ($wasRunning -and (Test-Path -LiteralPath $exe)) { Start-Process -FilePath $exe -WorkingDirectory $installRoot -WindowStyle Hidden }
        throw
    }
    $shell = New-Object -ComObject WScript.Shell
    foreach ($folder in @([Environment]::GetFolderPath('Desktop'), [Environment]::GetFolderPath('Programs'))) {
        $oldLink = Join-Path $folder '语音助手.lnk'
        if ((Test-Path -LiteralPath $oldLink) -and $shell.CreateShortcut($oldLink).TargetPath -eq $oldExe) { Remove-Item -LiteralPath $oldLink -Force }
        $link = $shell.CreateShortcut((Join-Path $folder 'VOA.lnk'))
        $link.TargetPath = $exe
        $link.WorkingDirectory = $installRoot
        $link.IconLocation = "$exe,0"
        $link.Description = 'VOA 语音助手'
        $link.Save()
    }
    $reg = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\VoiceAssistant'
    New-Item -Path $reg -Force | Out-Null
    $uninstall = 'powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File "' + (Join-Path $installRoot 'uninstall.ps1') + '"'
    $values = @{ DisplayName = 'VOA'; DisplayVersion = '0.1.0'; Publisher = 'VOA'; InstallLocation = $installRoot; DisplayIcon = $exe; UninstallString = $uninstall }
    foreach ($name in $values.Keys) { New-ItemProperty -Path $reg -Name $name -Value $values[$name] -PropertyType String -Force | Out-Null }
    foreach ($name in @('NoModify', 'NoRepair')) { New-ItemProperty -Path $reg -Name $name -Value 1 -PropertyType DWord -Force | Out-Null }
    # Notify Explorer that the executable icon and shortcuts have changed.
    if (-not ('VoiceAssistant.ShellIcons' -as [type])) {
        Add-Type -TypeDefinition 'using System; using System.Runtime.InteropServices; namespace VoiceAssistant { public static class ShellIcons { [DllImport("shell32.dll")] public static extern void SHChangeNotify(uint e, uint f, IntPtr a, IntPtr b); } }'
    }
    [VoiceAssistant.ShellIcons]::SHChangeNotify(0x08000000, 0, [IntPtr]::Zero, [IntPtr]::Zero)
    # Preserve an existing startup preference and migrate its executable path.
    $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
    if ((Get-ItemProperty -Path $runKey -Name VoiceAssistant -ErrorAction SilentlyContinue)) {
        Set-ItemProperty -Path $runKey -Name VoiceAssistant -Value ('"' + $exe + '" --autostart')
    }
    if ($Launch -or $wasRunning) { Start-Process -FilePath $exe -WorkingDirectory $installRoot -WindowStyle Hidden }
    Write-Host "已安装/更新：$exe"
} finally {
    $resolvedStage = [IO.Path]::GetFullPath($stage)
    $tempRoot = [IO.Path]::GetFullPath($env:TEMP).TrimEnd('\') + '\'
    if ($resolvedStage.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and (Split-Path $resolvedStage -Leaf) -like 'VoiceAssistant-install-*') {
        Remove-Item -LiteralPath $resolvedStage -Recurse -Force -ErrorAction SilentlyContinue
    }
}
