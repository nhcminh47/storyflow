param([string] $CliPath)
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$source = (Resolve-Path "$PSScriptRoot/../..").Path
if (-not $CliPath) {
    $exe = if ($IsWindows) { 'storyflow.exe' } else { 'storyflow' }
    $CliPath = "$source/target/debug/$exe"
}
$CliPath = (Resolve-Path -LiteralPath $CliPath).Path
$root = Join-Path ([IO.Path]::GetTempPath()) ("storyflow-matrix-" + [guid]::NewGuid())

function Invoke-Git {
    $out = & git @args
    if ($LASTEXITCODE -ne 0) { throw "git failed: $args" }
    $out
}
function Flow {
    param([int] $Exit, [Parameter(ValueFromRemainingArguments)] [string[]] $Arguments)
    $out = & $CliPath @Arguments 2>&1
    if ($LASTEXITCODE -ne $Exit) { throw "Expected exit $Exit for $Arguments, got ${LASTEXITCODE}: $out" }
    if ($Exit -ne 2) { ($out -join [Environment]::NewLine) | ConvertFrom-Json }
}
function Assert-Case($Condition, $Message) {
    if (-not $Condition) { throw "${id}: $Message" }
}
function Commit-Value($Value) {
    Set-Content app/value.txt $Value
    Invoke-Git add --all
    Invoke-Git commit -qm "Value $Value"
    Invoke-Git rev-parse HEAD
}
function Submit-Candidate($Commit) {
    $state = Flow 0 status $id
    $null = Flow 0 submit $id $state.revision candidate report.md $Commit
}
function Run-Gate($Exit) {
    $state = Flow 0 status $id
    Flow $Exit gate $id $state.revision
}

foreach ($id in @('scope-001','review-001','retry-001','timeout-001','startup-001','candidate-001','journal-001')) {
    $run = (New-Item -ItemType Directory -Path (Join-Path $root $id) -Force).FullName
    Push-Location $run
    try {
        New-Item -ItemType Directory app,stories | Out-Null
        Copy-Item "$source/stories/$id" stories -Recurse
        Copy-Item "$source/agents" . -Recurse
        @('.storyflow/', 'report.md', 'retry-ready') | Set-Content .gitignore
        'initial' | Set-Content app/value.txt
        'fixed' | Set-Content app/protected.txt
        'fixed' | Set-Content app/sibling.txt
        'Matrix fixture report' | Set-Content report.md
        @'
param($Mode, $Ready)
$ErrorActionPreference = 'Stop'
if ($Mode -eq 'timeout' -and -not (Test-Path -LiteralPath $Ready)) {
    Start-Sleep -Seconds 15
}
if ((Get-Content app/value.txt -Raw).Trim() -cnotin @('ok', 'better')) { exit 1 }
'@ | Set-Content verify.ps1

        $mode = if ($id -eq 'timeout-001') { 'timeout' } else { 'normal' }
        $program = if ($id -eq 'startup-001') { "missing-storyflow-" + [guid]::NewGuid() } else { (Get-Command pwsh).Source }
        $seconds = if ($mode -eq 'timeout') { 4 } else { 30 }
        @{
            version = 1
            protected = @('app/protected.txt', 'verify.ps1')
            checks = @(@{
                id = 'acceptance'
                program = $program
                args = @('-NoProfile','-File','verify.ps1',$mode,(Join-Path $run 'retry-ready'))
                timeout_seconds = $seconds
            })
        } | ConvertTo-Json -Depth 5 | Set-Content storyflow.json

        Invoke-Git init -q
        Invoke-Git config user.name 'Storyflow Matrix'
        Invoke-Git config user.email 'storyflow-matrix@example.invalid'
        Invoke-Git config commit.gpgsign false
        $hooks = (New-Item -ItemType Directory .git/empty-hooks).FullName
        Invoke-Git config core.hooksPath $hooks
        Invoke-Git add --all
        Invoke-Git commit -qm Baseline
        $base = Invoke-Git rev-parse HEAD
        $null = Flow 0 init $id
        $null = Flow 0 submit $id 0 plan report.md

        switch ($id) {
            'scope-001' {
                foreach ($path in @('app/sibling.txt','app/protected.txt')) {
                    'changed' | Set-Content $path
                    Submit-Candidate (Commit-Value ok)
                    $result = Run-Gate 1
                    Assert-Case ($result.checks.Count -eq 1 -and $result.checks[0].detail.Contains($path)) "Expected scope rejection for $path"
                    Invoke-Git restore "--source=$base" -- $path
                }
                Invoke-Git config diff.ignoreSubmodules all
                Invoke-Git update-index --add --cacheinfo "160000,$base,app/unscoped-submodule"
                Invoke-Git commit -qm 'Add unscoped gitlink'
                Submit-Candidate (Invoke-Git rev-parse HEAD)
                $result = Run-Gate 1
                Assert-Case ($result.checks.Count -eq 1 -and $result.checks[0].detail.Contains('app/unscoped-submodule')) 'Git config hid a changed gitlink'
                Invoke-Git rm --cached -- app/unscoped-submodule | Out-Null
                Submit-Candidate (Commit-Value better)
                Assert-Case ((Run-Gate 0).status.stage -eq 'review') 'Scoped correction did not pass'
            }
            'review-001' {
                $first = Commit-Value ok
                Submit-Candidate $first
                $passed = Run-Gate 0
                Flow 2 submit $id 1 changes report.md $first
                $change = Flow 0 submit $id $passed.status.revision changes report.md $first
                Assert-Case ($change.stage -eq 'implement' -and $change.rejected_candidates -eq 1) 'Review changes did not reopen implementation'
                $second = Commit-Value better
                Submit-Candidate $second
                $passed = Run-Gate 0
                Flow 2 submit $id $passed.status.revision approve report.md $first
                $done = Flow 0 submit $id $passed.status.revision approve report.md $second
                Assert-Case ($done.stage -eq 'done') 'Replacement candidate not approved'
            }
            'retry-001' {
                foreach ($attempt in 1..3) {
                    Submit-Candidate (Commit-Value "bad-$attempt")
                    $null = Run-Gate 1
                    $state = Flow 0 status $id
                    Assert-Case ($state.rejected_candidates -eq $attempt) 'Rejection counter mismatch'
                }
                $null = Flow 0 submit $id $state.revision block report.md
                Assert-Case ((Flow 0 next $id).action -eq 'resume') 'Retry block not persisted'
            }
            { $_ -in @('timeout-001','startup-001') } {
                $candidate = Commit-Value ok
                Submit-Candidate $candidate
                $result = Run-Gate 1
                $expected = if ($id -eq 'timeout-001') { 'timeout' } else { 'error' }
                Assert-Case ($result.checks[-1].outcome -eq $expected) "Expected $expected"
                $next = Flow 0 next $id
                $preserved = $next.status.stage -eq 'gate' -and $next.status.candidate -eq $candidate
                Assert-Case ($next.action -eq 'resume' -and $next.blocker -and $preserved -and $next.status.rejected_candidates -eq 0) 'Operational block did not preserve candidate'
                if ($id -eq 'timeout-001') {
                    New-Item retry-ready | Out-Null
                    $null = Flow 0 submit $id $next.status.revision resume report.md
                    Assert-Case ((Run-Gate 0).status.stage -eq 'review') 'Resumed gate did not pass'
                }
            }
            'candidate-001' {
                Flow 2 submit $id 1 candidate report.md $base
                $tree = Invoke-Git rev-parse 'HEAD^{tree}'
                $unrelated = Invoke-Git commit-tree $tree -m Unrelated
                Flow 2 submit $id 1 candidate report.md $unrelated
                Invoke-Git commit --allow-empty -qm Empty
                Submit-Candidate (Invoke-Git rev-parse HEAD)
                $result = Run-Gate 1
                Assert-Case ($result.checks.Count -eq 1 -and $result.checks[0].detail -eq 'candidate has no tree changes') 'Empty candidate not rejected'
            }
            'journal-001' {
                $event = Get-ChildItem ".storyflow/$id/events" -Filter '*.json' | Sort-Object Name | Select-Object -Last 1
                $original = [IO.File]::ReadAllBytes($event.FullName)
                $record = Get-Content $event.FullName -Raw | ConvertFrom-Json
                $record.sha256 = '0' * 64
                $record | ConvertTo-Json -Depth 20 | Set-Content $event.FullName
                try { Flow 2 status $id }
                finally { [IO.File]::WriteAllBytes($event.FullName, $original) }
                Assert-Case ((Flow 0 status $id).revision -eq 1) 'Journal did not replay after restoration'
            }
        }
        [pscustomobject]@{ story = $id; result = 'pass'; workspace = $run }
    } finally { Pop-Location }
}
