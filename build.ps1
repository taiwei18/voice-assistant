# 构建脚本：加载 VS 编译环境（让链接器找到 Windows SDK），编译后把模型复制到 exe 旁边。
# 用法：  .\build.ps1            # 调试版
#        .\build.ps1 -Release   # 发布版，并把能直接运行的整套文件放到 dist\（双击 dist\voa.exe 即可）
param([switch]$Release, [switch]$Run, [switch]$Test)

$ErrorActionPreference = 'Stop'
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
if (-not $env:CARGO_TARGET_DIR) { $env:CARGO_TARGET_DIR = "$env:USERPROFILE\.cargo-target\va" }  # 短路径，避开 260 字符限制

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$vs = & $vswhere -latest -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
$vcvars = Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat'
cmd /c "`"$vcvars`" >nul && set" | ForEach-Object {
    if ($_ -match '^(PATH|LIB|LIBPATH|INCLUDE|WindowsSdkDir|WindowsSDKVersion|VCToolsInstallDir)=(.*)$') {
        Set-Item -Path "env:$($Matches[1])" -Value $Matches[2]
    }
}
# 部分电脑上 SDK 注册信息不全，vcvars 只加了 ucrt，没加 um（kernel32.lib 在这里），手动补上
$kits = "${env:ProgramFiles(x86)}\Windows Kits\10"
$sdk = Get-ChildItem "$kits\Lib" -Directory | Where-Object { Test-Path "$($_.FullName)\um\x64\kernel32.lib" } |
    Sort-Object Name -Descending | Select-Object -First 1
if ($sdk -and $env:LIB -notmatch [regex]::Escape("$($sdk.Name)\um\x64")) {
    $env:LIB = "$env:LIB;$($sdk.FullName)\um\x64;$($sdk.FullName)\ucrt\x64"
    $inc = "$kits\Include\$($sdk.Name)"
    $env:INCLUDE = "$env:INCLUDE;$inc\um;$inc\shared;$inc\ucrt;$inc\winrt"
    $env:PATH = "$env:PATH;$kits\bin\$($sdk.Name)\x64"
}

Push-Location $PSScriptRoot
try {
    [string[]]$profileArgs = @()
    if ($Release) { $profileArgs += '--release' }
    if ($Test) {
        cargo test @profileArgs
        exit $LASTEXITCODE
    }
    cargo build @profileArgs
    if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
    $outDir = Join-Path $env:CARGO_TARGET_DIR ($(if ($Release) { 'release' } else { 'debug' }))
    # 只复制实际加载的模型文件（int8 编码器/连接器 + 解码器 + tokens），约 5MB
    $modelsOut = Join-Path $outDir 'models'
    if (Test-Path $modelsOut) { Remove-Item -Recurse -Force $modelsOut }
    robocopy models $modelsOut /E /NFL /NDL /NJH /NJS /XD test_wavs `
        /XF *chunk-8* en.phone encoder-epoch-13-avg-2-chunk-16-left-64.onnx joiner-epoch-13-avg-2-chunk-16-left-64.onnx | Out-Null
    if ($LASTEXITCODE -ge 8) { throw "复制模型失败（robocopy $LASTEXITCODE）" }
    $global:LASTEXITCODE = 0  # robocopy 用 1 表示“已复制”，不是错误
    $exe = Join-Path $outDir 'voa.exe'
    if ($Release) {
        # 发布版放到固定的 dist\：开机自启登记的就是这里的路径，以后重新编译也不会变
        $dist = Join-Path $PSScriptRoot 'dist'
        $running = Get-Process voa, voice-assistant -ErrorAction SilentlyContinue | Where-Object { $_.Path -like "$dist\*" }
        if ($running) {
            $running | Stop-Process -Force
            Start-Sleep -Milliseconds 500
            Write-Host "已关闭正在运行的旧版本"
        }
        robocopy $outDir $dist voa.exe *.dll /NFL /NDL /NJH /NJS | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "复制到 dist 失败（robocopy $LASTEXITCODE）" }
        Remove-Item -LiteralPath (Join-Path $dist 'voice-assistant.exe') -ErrorAction SilentlyContinue  # 改名前的旧程序
        robocopy $modelsOut (Join-Path $dist 'models') /MIR /NFL /NDL /NJH /NJS | Out-Null
        if ($LASTEXITCODE -ge 8) { throw "复制模型到 dist 失败（robocopy $LASTEXITCODE）" }
        $global:LASTEXITCODE = 0
        $exe = Join-Path $dist 'voa.exe'
        # Once installed, every release build also updates the installed application.
        $installedMarker = Join-Path $env:LOCALAPPDATA 'Programs\VoiceAssistant\installation.json'
        if (Test-Path -LiteralPath $installedMarker) {
            & (Join-Path $PSScriptRoot 'install.ps1') -Source $dist -Launch
            $exe = Join-Path $env:LOCALAPPDATA 'Programs\VoiceAssistant\voa.exe'
            $Run = $false
        }
    }
    Write-Host "已生成：$exe"
    if ($Run) { & $exe }
} finally {
    Pop-Location
}
