# mincore wasm32 逐位对拍的完整编排（Windows PowerShell 5.1）
#
# 用法：  .\run-all.ps1         （在任意目录下都能跑，用 $PSScriptRoot 解析仓库根）
#
# 为什么需要这个脚本而不是一条 node 命令：
# 本机沙箱禁止 Node 用管道打开子进程（child_process 默认 stdio:'pipe' -> EPERM）。
# 所以 native 侧由 PowerShell 跑并落盘，node 只负责读文件 + 跑 wasm + 对拍。
#
# 注意：这个文件**必须是 UTF-8 带 BOM**，否则 PS 5.1 会按 GBK 解码中文导致语法错误。
# （tools/wasm-smoke/run-all.ps1 同一个坑，解法照抄。）

[Console]::OutputEncoding = [System.Text.Encoding]::UTF8

$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$env:TMP = Join-Path $root '.tmp'
$env:TEMP = $env:TMP
if (-not (Test-Path -LiteralPath $env:TMP)) {
    New-Item -ItemType Directory -Path $env:TMP -Force | Out-Null
}

# bin 名与 Cargo.toml 保持一致（lib 名不同，否则 cargo 会报 output filename collision）
$binName = 'mc_wasm_cli'

Push-Location -LiteralPath $PSScriptRoot
try {
    Write-Host "== 1/6 编 wasm32 lib（产出 .wasm，run-compare.mjs 读它）=="
    cargo build --release --lib --target wasm32-unknown-unknown | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'wasm32 编译失败' }

    Write-Host "== 2/6 编 native bin（同一个 run_all()，不是另一份实现）=="
    cargo build --release --bin $binName | Out-Null
    if ($LASTEXITCODE -ne 0) { throw 'native 编译失败' }

    Write-Host "== 3/6 跑 native 并落盘 native.txt =="
    $exe = Join-Path $PSScriptRoot "target\release\$binName.exe"
    $txt = Join-Path $PSScriptRoot 'native.txt'
    $lines = (& $exe) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'native 运行失败（探针内部断言没过？）' }
    # 用 .NET 直接写无 BOM 的 UTF-8，避开 PS5.1 的 Out-File 会插 BOM 的问题
    [System.IO.File]::WriteAllText($txt, $lines + "`n", [System.Text.UTF8Encoding]::new($false))

    Write-Host "== 4/6 确定性自检：native 再跑一遍，两遍必须逐字节相同 =="
    $lines2 = (& $exe) -join "`n"
    if ($LASTEXITCODE -ne 0) { throw 'native 第二遍运行失败' }
    $txt2 = Join-Path $PSScriptRoot 'native2.txt'
    [System.IO.File]::WriteAllText($txt2, $lines2 + "`n", [System.Text.UTF8Encoding]::new($false))
    $a = [System.IO.File]::ReadAllBytes($txt)
    $b = [System.IO.File]::ReadAllBytes($txt2)
    $same = ($a.Length -eq $b.Length)
    if ($same) {
        for ($i = 0; $i -lt $a.Length; $i++) {
            if ($a[$i] -ne $b[$i]) { $same = $false; break }
        }
    }
    Remove-Item -LiteralPath $txt2 -Force -ErrorAction SilentlyContinue
    if (-not $same) { throw 'native 自己跑两遍结果不同（探针在抖动，对拍结论不可信）' }
    Write-Host "   native 两遍逐字节相同（$($a.Length) 字节）"

    Write-Host "== 5/6 负对照：人为篡改 1 个位模式 + 1 个快照字节，对拍器必须报差异 =="
    # 为什么需要这一步：对拍脚本自己也可能永远打印 IDENTICAL（假阳）。
    # 每次跑都必须证明它真的抓得住人为差异。
    $raw = [System.IO.File]::ReadAllText($txt, [System.Text.UTF8Encoding]::new($false))
    $rawLines = $raw -split "`n"
    $bitsLine = $rawLines[1]
    $badBits = ([regex]'0x00000000').Replace($bitsLine, '0x10000000', 1)
    if ($badBits -eq $bitsLine) { throw '负对照：位模式行里找不到可篡改的 0x00000000' }
    $snapLine = $rawLines[4]
    if ($snapLine.Length -lt 2) { throw '负对照：第一张快照行是空的' }
    $firstByte = $snapLine.Substring(0, 2)
    $replacement = if ($firstByte -eq 'ff') { 'ee' } else { 'ff' }
    $badSnap = $replacement + $snapLine.Substring(2)
    $badLines = $raw -split "`n"
    $badLines[1] = $badBits
    $badLines[4] = $badSnap
    $badTxt = Join-Path $PSScriptRoot 'native_bad.txt'
    [System.IO.File]::WriteAllText($badTxt, ($badLines -join "`n"), [System.Text.UTF8Encoding]::new($false))
    node (Join-Path $PSScriptRoot 'run-compare.mjs') $badTxt
    $badCode = $LASTEXITCODE
    Remove-Item -LiteralPath $badTxt -Force -ErrorAction SilentlyContinue
    if ($badCode -eq 0) { throw '负对照失败：对拍器没有发现人为篡改（对拍逻辑是假阳）' }
    Write-Host "   负对照通过：对拍器正确报出差异（退出码 $badCode）"

    Write-Host "== 6/6 正式对拍：跑 wasm 并与 native 逐位比较 =="
    node (Join-Path $PSScriptRoot 'run-compare.mjs') $txt
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
