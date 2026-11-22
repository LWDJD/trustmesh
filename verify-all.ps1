# 全量验证：一条命令跑完所有该跑的东西
#
# 用法：  .\verify-all.ps1
#
# 这个脚本存在的原因：本仓库是**七个独立 workspace**（store/view/agg/engine/sim/exp/mincore）+ 根 workspace，
# 每个 crate 都要单独 cargo test。新人不知道这个结构就会只跑根目录的 58 条测试然后以为全绿了。
#
# 检查项：
#   1. 单元/集成测试（七个 workspace 各自 cargo test）
#   2. 文档测试（cargo test --doc）
#   3. clippy（库与测试零警告；exp 的历史实验 bin 不强制）
#      七个 crate 逐个跑，mincore 也进来了
#   4. wasm32 逐位对拍（tools/wasm-smoke + tools/mincore-wasm）
#
# 退出码：0 = 全绿；非 0 = 有失败项（并打印是哪一项）。
#
# 注意：这个文件必须 UTF-8 带 BOM，否则 PS5.1 按 GBK 解码中文会语法错误。

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = (Resolve-Path (Join-Path $PSScriptRoot '.')).Path
$env:TMP = Join-Path $root '.tmp'
$env:TEMP = $env:TMP
if (-not (Test-Path -LiteralPath $env:TMP)) { New-Item -ItemType Directory -Path $env:TMP -Force | Out-Null }

$script:failed = @()
# 2026-11-21 合并后：store / view / agg 已并入 engine（三个原 crate 目录已删除）。
# 四个 workspace：engine / sim / exp / mincore + 根 v1.0。
$crates = @('engine', 'sim', 'exp', 'mincore')

function Report($name, $ok, $detail) {
    if ($ok) {
        Write-Host ("  [OK]   {0,-30} {1}" -f $name, $detail) -ForegroundColor Green
    } else {
        Write-Host ("  [FAIL] {0,-30} {1}" -f $name, $detail) -ForegroundColor Red
        $script:failed += $name
    }
}

function Sum-Tests($out) {
    $m = [regex]::Matches($out, 'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed; (\d+) ignored')
    $p = 0; $f = 0; $i = 0
    foreach ($x in $m) { $p += [int]$x.Groups[2].Value; $f += [int]$x.Groups[3].Value; $i += [int]$x.Groups[4].Value }
    return @($p, $f, $i)
}

Write-Host "仓库根：$root" -ForegroundColor Cyan
Write-Host ""

Write-Host "[1/5] 测试" -ForegroundColor Cyan

foreach ($c in $crates) {
    Push-Location -LiteralPath (Join-Path $root ("crates/" + $c))
    try {
        $out = (cargo test 2>&1 | Out-String)
        $s = Sum-Tests $out
        Report $c ($s[1] -eq 0) ("passed={0} failed={1} ignored={2}" -f $s[0], $s[1], $s[2])
    } finally { Pop-Location }
}
Push-Location -LiteralPath $root
try {
    $out = (cargo test 2>&1 | Out-String)
    $s = Sum-Tests $out
    Report 'trustmesh(v1.0)' ($s[1] -eq 0) ("passed={0} failed={1} ignored={2}" -f $s[0], $s[1], $s[2])
} finally { Pop-Location }
Write-Host ""

Write-Host "[2/5] 文档测试" -ForegroundColor Cyan
foreach ($c in $crates) {
    Push-Location -LiteralPath (Join-Path $root ("crates/" + $c))
    try {
        $out = (cargo test --doc 2>&1 | Out-String)
        $m = [regex]::Matches($out, 'test result: (ok|FAILED)\. (\d+) passed; (\d+) failed')
        $p = 0; $f = 0
        foreach ($x in $m) { $p += [int]$x.Groups[2].Value; $f += [int]$x.Groups[3].Value }
        if ($p -gt 0) { Report ("$c (doc)") ($f -eq 0) ("passed={0} failed={1}" -f $p, $f) }
    } finally { Pop-Location }
}
Write-Host ""

Write-Host "[3/5] clippy" -ForegroundColor Cyan
# exp 的 21 个历史实验 bin 有约 50 条遗留警告（未读字段、过时变量等），
# 那些是一次性实验代码，改它们的应该是重跑实验的人。所以 exp 只对 lib 与 tests 严格。
foreach ($c in $crates) {
    Push-Location -LiteralPath (Join-Path $root ("crates/" + $c))
    try {
        if ($c -eq 'exp') {
            # exp 只卡 lib + 两个集成测试。那 21 个实验 bin 是一次性脚本，
            # 里面有 46 条历史警告（未读字段、过时变量），改它们属于「重跑实验的人」的活。
            $out = (cargo clippy --lib --test runtime --test parity 2>&1 | Out-String)
        } elseif ($c -eq 'mincore') {
            # mincore 的 lib + tests + doctest 严格零警告；两个对照实验 bin（a12_compare /
            # a12_perf）在 2026-11-18 已被清到零，所以这里也用 --all-targets。
            $out = (cargo clippy --all-targets 2>&1 | Out-String)
        } else {
            $out = (cargo clippy --all-targets 2>&1 | Out-String)
        }
        $w = ([regex]::Matches($out, '(?m)^warning: ')).Count
        $gen = ([regex]::Matches($out, '(?m)^warning: [`]')).Count
        Report $c (($w - $gen) -eq 0) ("warnings={0}" -f ($w - $gen))
    } finally { Pop-Location }
}
Write-Host ""

Write-Host "[4/5] cargo fmt --check" -ForegroundColor Cyan
foreach ($c in $crates) {
    Push-Location -LiteralPath (Join-Path $root ("crates/" + $c))
    try {
        $out = (cargo fmt --check 2>&1 | Out-String)
        $d = ([regex]::Matches($out, '(?m)^Diff in ')).Count
        Report $c ($d -eq 0) ("diffs={0}" -f $d)
    } finally { Pop-Location }
}
Write-Host ""

Write-Host "[5/5] wasm32 逐位对拍（engine + mincore 两套）" -ForegroundColor Cyan
# engine 的对拍：tools/wasm-smoke/run-all.ps1
$wasm = Join-Path $root 'tools/wasm-smoke/run-all.ps1'
$out = (& $wasm 2>&1 | Out-String)
$ok = ($LASTEXITCODE -eq 0)
Report 'wasm32 vs native (engine)' $ok ($(if ($ok) { 'IDENTICAL' } else { 'DIFFERS' }))

# mincore 的对拍：tools/mincore-wasm/run-all.ps1（2026-11-18 新增）
$wasm2 = Join-Path $root 'tools/mincore-wasm/run-all.ps1'
if (Test-Path -LiteralPath $wasm2) {
    $out = (& $wasm2 2>&1 | Out-String)
    $ok = ($LASTEXITCODE -eq 0)
    Report 'wasm32 vs native (mincore)' $ok ($(if ($ok) { 'IDENTICAL' } else { 'DIFFERS' }))
} else {
    Write-Host '  [SKIP] tools/mincore-wasm 不存在' -ForegroundColor Yellow
}
Write-Host ""

if ($script:failed.Count -eq 0) {
    Write-Host "全部通过" -ForegroundColor Green
    exit 0
} else {
    Write-Host ("有 {0} 项失败：{1}" -f $script:failed.Count, ($script:failed -join ', ')) -ForegroundColor Red
    exit 1
}