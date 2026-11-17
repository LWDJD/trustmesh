# wasm32 逐位对拍的完整编排（Windows PowerShell 5.1）
#
# 用法：  .\run-all.ps1         （在任意目录下都能跑，用 $PSScriptRoot 解析仓库根）
#
# 为什么需要这个脚本而不是一条 node 命令：
# 本机沙箱禁止 Node 用管道打开子进程（child_process 默认 stdio:'pipe' → EPERM）。
# 所以 native 侧由 PowerShell 跑并落盘，node 只负责读文件 + 跑 wasm + 对拍。
#
# 注意：这个文件**必须是 UTF-8 带 BOM**，否则 PS 5.1 会按 GBK 解码中文导致语法错误。

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$env:TMP = Join-Path $root '.tmp'
$env:TEMP = $env:TMP
if (-not (Test-Path -LiteralPath $env:TMP)) {
    New-Item -ItemType Directory -Path $env:TMP -Force | Out-Null
}

Push-Location -LiteralPath $PSScriptRoot
try {
    Write-Host "== 1/4 编 wasm32 =="
    cargo build --release --lib --target wasm32-unknown-unknown | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'wasm 编译失败' }

    Write-Host "== 2/4 编 native =="
    cargo build --release --bin tm_wasm_smoke_cli | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'native 编译失败' }

    Write-Host "== 3/4 跑 native 并落盘 =="
    $exe = Join-Path $PSScriptRoot "target\release\tm_wasm_smoke_cli.exe"
    $txt = Join-Path $PSScriptRoot "native.txt"
    $lines = (& $exe) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'native 运行失败' }
    # 用 .NET 直接写无 BOM 的 UTF-8，避开 PS5.1 的 Out-File 会插 BOM 的问题
    [System.IO.File]::WriteAllText($txt, $lines + "`n", [System.Text.UTF8Encoding]::new($false))

    Write-Host "== 4/4 对拍 =="
    node (Join-Path $PSScriptRoot "run-compare.mjs") $txt
    $code = $LASTEXITCODE
    Write-Host ""
    if ($code -eq 0) {
        Write-Host "结果：逐位相同" -ForegroundColor Green
    } else {
        Write-Host "结果：存在差异（退出码 $code）" -ForegroundColor Red
    }
    exit $code
}
finally {
    Pop-Location
}
