#!/usr/bin/env pwsh
# mincore 一键验收：测试 + doctest + clippy + fmt + 例子 + wasm 对拍（若有）。
#
# 为什么单独一个脚本：mincore 是独立 workspace，根目录的 cargo test 不覆盖它。
# 而它的 4 项工程门槛（单测 / clippy / fmt / wasm）在不同 target 下检查，
# 命令各不相同，容易漏。
#
# 用法：pwsh -File verify-mincore.ps1
# 退出码：0 = 全绿。

$ErrorActionPreference = "Continue"
$root = (Resolve-Path (Join-Path $PSScriptRoot ".")).Path
$env:TMP = Join-Path $root ".tmp"
$env:TEMP = $env:TMP
if (-not (Test-Path -LiteralPath $env:TMP)) { New-Item -ItemType Directory -Path $env:TMP -Force | Out-Null }

$mc = Join-Path $root "crates/mincore"
$failed = @()
function Step($name, $ok, $detail) {
    if ($ok) { Write-Host ("  [OK]   {0,-34} {1}" -f $name, $detail) -ForegroundColor Green }
    else { Write-Host ("  [FAIL] {0,-34} {1}" -f $name, $detail) -ForegroundColor Red; $script:failed += $name }
}

Push-Location -LiteralPath $mc
try {
    Write-Host "[1/7] 集成测试 + doctest" -ForegroundColor Cyan
    $out = (cargo test 2>&1 | Out-String)
    $m = [regex]::Matches($out, "test result: (ok|FAILED)\. (\d+) passed; (\d+) failed")
    $p = 0; $f = 0
    foreach ($x in $m) { $p += [int]$x.Groups[2].Value; $f += [int]$x.Groups[3].Value }
    Step "cargo test" ($f -eq 0) ("passed={0} failed={1}" -f $p, $f)

    Write-Host "[2/7] clippy（库 + 测试 + 两个实验 bin）" -ForegroundColor Cyan
    $out = (cargo clippy --all-targets 2>&1 | Out-String)
    $w = ([regex]::Matches($out, "(?m)^warning: ")).Count
    Step "cargo clippy --all-targets" ($w -eq 0) ("warnings={0}" -f $w)

    Write-Host "[3/7] fmt" -ForegroundColor Cyan
    $out = (cargo fmt --check 2>&1 | Out-String)
    $d = ([regex]::Matches($out, "(?m)^Diff in ")).Count
    Step "cargo fmt --check" ($d -eq 0) ("diffs={0}" -f $d)

    Write-Host "[4/7] 例子可跑" -ForegroundColor Cyan
    $out = (cargo run --release --example quickstart 2>&1 | Out-String)
    Step "quickstart" ($LASTEXITCODE -eq 0) ("exit={0}" -f $LASTEXITCODE)
    $out = (cargo run --release --example cap_loss 2>&1 | Out-String)
    Step "cap_loss" ($LASTEXITCODE -eq 0) ("exit={0}" -f $LASTEXITCODE)
} finally { Pop-Location }

Write-Host "[5/7] 工作区干净（测试不应产生未提交改动）" -ForegroundColor Cyan
$out = (git -C $root status --porcelain 2>&1 | Out-String)
$dirty = ($out -split [Environment]::NewLine | Where-Object { $_ -match "^ M|^M |^A " }).Count
Step "工作区干净（测试不应改文件）" ($dirty -eq 0) ("dirty={0}" -f $dirty)

Write-Host "[6/7] wasm32 逐位对拍（若 tools/mincore-wasm 存在）" -ForegroundColor Cyan
$wasm = Join-Path $root "tools/mincore-wasm"
if (Test-Path -LiteralPath $wasm) {
    Push-Location -LiteralPath $wasm
    try {
        $out = (cargo build --release --lib --target wasm32-unknown-unknown 2>&1 | Out-String)
        Step "wasm32 编译（lib）" ($LASTEXITCODE -eq 0) ("exit={0}" -f $LASTEXITCODE)
        if (Test-Path (Join-Path $root "tools/mincore-wasm/native.txt")) {
            $out = (node run-compare.mjs native.txt 2>&1 | Out-String)
            $identical = $out -match "IDENTICAL"
            Step "wasm vs native 逐位" $identical ("exit={0}" -f $LASTEXITCODE)
        } else {
            Step "native.txt 缺失" $false "先跑 cargo run --release --bin <cli> > native.txt"
        }
    } finally { Pop-Location }
} else {
    Write-Host "  [SKIP] tools/mincore-wasm 不存在（wasm 对拍尚未补齐）" -ForegroundColor Yellow
}

Write-Host "[7/7] 与现行实现的 S1~S6 对照（慢，约 1 分钟）" -ForegroundColor Cyan
Write-Host "  跳过。需要时手动跑：cargo run --release --bin a12_compare" -ForegroundColor Yellow

Write-Host ""
if ($script:failed.Count -eq 0) {
    Write-Host "全部通过。" -ForegroundColor Green
    exit 0
} else {
    Write-Host ("{0} 项失败：{1}" -f $script:failed.Count, ($script:failed -join ", ")) -ForegroundColor Red
    exit 1
}