# wasm32 逐位对拍的完整编排（Windows PowerShell 5.1）
#
# 用法：  cd tools/wasm-smoke ;  .\run-all.ps1
#
# 为什么需要这个脚本而不是一条 node 命令：
# 本机沙箱禁止 Node 用管道打开子进程（child_process 默认 stdio:'pipe' → EPERM）。
# 所以 native 侧由 PowerShell 跑并落盘，node 只负责读文件 + 跑 wasm + 对拍。

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8
$ErrorActionPreference = "Stop"

$root = (Resolve-Path "..\..").Path
$env:TMP = "$root\.tmp"
$env:TEMP = $env:TMP
if (-not (Test-Path $env:TMP)) { New-Item -ItemType Directory -Path $env:TMP -Force | Out-Null }

Push-Location -LiteralPath (Join-Path $root "tools\wasm-smoke")
try {
    Write-Host "== 1/4 编 wasm32 =="
    cargo build --release --lib --target wasm32-unknown-unknown 2>&1 | Select-Object -Last 2
    if ($LASTEXITCODE -ne 0) { throw "wasm 编译失败" }

    Write-Host "== 2/4 编 native =="
    cargo build --release --bin tm_wasm_smoke 2>&1 | Select-Object -Last 2
    if ($LASTEXITCODE -ne 0) { throw "native 编译失败" }

    Write-Host "== 3/4 跑 native 并落盘 =="
    $native = "./target/release/tm_wasm_smoke.exe"
    # 用 .NET 直接写，避开 PS5.1 的 Out-File 会插 BOM 的问题
    $lines = (& $native) -join "`n"
    [System.IO.File]::WriteAllText((Join-Path (Get-Location).Path "native.txt"), $lines + "`n", [System.Text.UTF8Encoding]::new($false))
    if ($LASTEXITCODE -ne 0) { throw "native 运行失败" }

    Write-Host "== 4/4 对拍 =="
    node run-compare.mjs native.txt
    $code = $LASTEXITCODE
    Write-Host ""
    if ($code -eq 0) { Write-Host "结果：逐位相同" -ForegroundColor Green }
    else { Write-Host "结果：存在差异（退出码 $code）" -ForegroundColor Red }
    exit $code
}
finally {
    Pop-Location
}
