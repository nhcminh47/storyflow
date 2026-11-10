[CmdletBinding()]
param(
    [string] $CliPath,
    [string] $RunDirectory
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$repo = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
$id = 'slug-001'
$fixtures = Join-Path $PSScriptRoot 'fixtures'
$appFile = 'examples/slug/app/src/lib.rs'

if (-not $CliPath) {
    $exe = if ($IsWindows) { 'storyflow.exe' } else { 'storyflow' }
    $CliPath = Join-Path $repo "target/debug/$exe"
}
$CliPath = (Resolve-Path -LiteralPath $CliPath).Path
if (-not $RunDirectory) {
    $RunDirectory = Join-Path ([IO.Path]::GetTempPath()) ('storyflow-demo-' + [guid]::NewGuid())
}
if (Test-Path -LiteralPath $RunDirectory) {
    throw "RunDirectory must be a new directory: $RunDirectory"
}
$run = (New-Item -ItemType Directory -Path $RunDirectory).FullName

function Invoke-Git {
    $output = & git @args
    if ($LASTEXITCODE -ne 0) { throw "git failed: $args" }
    $output
}

function Invoke-Flow {
    param([int] $ExpectedExit, [Parameter(ValueFromRemainingArguments)] [string[]] $Arguments)
    $output = & $CliPath @Arguments 2>&1
    if ($LASTEXITCODE -ne $ExpectedExit) {
        throw "Unexpected exit for storyflow $Arguments ($LASTEXITCODE): $output"
    }
    if ($ExpectedExit -ne 2) { ($output -join "`n") | ConvertFrom-Json }
}

function Assert-That {
    param([bool] $Condition, [string] $Message)
    if (-not $Condition) { throw $Message }
}

foreach ($item in @('.gitignore', '.gitattributes', 'AGENTS.md', 'CLAUDE.md', 'agents', 'stories', 'storyflow.json')) {
    Copy-Item -LiteralPath (Join-Path $repo $item) -Destination $run -Recurse
}
$example = New-Item -ItemType Directory -Path (Join-Path $run 'examples/slug')
foreach ($item in @('app', 'gate')) {
    # Copy tracked fixture inputs only, including the lockfile.
    $destination = New-Item -ItemType Directory -Path (Join-Path $example.FullName $item)
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "$item/Cargo.toml") -Destination $destination.FullName
    $sources = if ($item -eq 'app') { 'src' } else { 'tests' }
    Copy-Item -LiteralPath (Join-Path $PSScriptRoot "$item/$sources") -Destination $destination.FullName -Recurse
}
Copy-Item -LiteralPath (Join-Path $PSScriptRoot 'gate/Cargo.lock') -Destination (Join-Path $example.FullName 'gate')

Push-Location $run
try {
    Invoke-Git init --quiet
    Invoke-Git config user.name 'Storyflow Demo'
    Invoke-Git config user.email 'storyflow-demo@example.invalid'
    Invoke-Git config commit.gpgsign false
    $emptyHooks = (New-Item -ItemType Directory -Path '.git/empty-hooks').FullName
    Invoke-Git config core.hooksPath $emptyHooks
    Invoke-Git add --all
    Invoke-Git commit --quiet -m 'Baseline for SLUG-001'

    $init = Invoke-Flow 0 init $id HEAD
    $planner = Invoke-Flow 0 next $id
    Assert-That ($planner.role -eq 'planner') 'Planner dispatch missing'
    $plan = Join-Path $fixtures 'plan.md'
    $implementation = Invoke-Flow 0 submit $id $init.revision plan $plan
    Invoke-Flow 2 submit $id $init.revision plan $plan
    $worker = Invoke-Flow 0 next $id
    Assert-That ($worker.role -eq 'implementer' -and $worker.plan) 'Plan handoff missing'

    [IO.File]::AppendAllText((Join-Path $run $appFile), "// First attempt leaves behavior unchanged.`n")
    Invoke-Git add -- $appFile
    Invoke-Git commit --quiet -m 'First candidate'
    $firstCommit = Invoke-Git rev-parse HEAD
    $firstReport = Join-Path $fixtures 'first-attempt.md'
    $first = Invoke-Flow 0 submit $id $implementation.revision candidate $firstReport $firstCommit
    $failure = Invoke-Flow 1 gate $id $first.revision
    Assert-That ($failure.checks[-1].id -eq 'acceptance' -and $failure.checks[-1].outcome -eq 'fail') 'Expected acceptance failure'
    Assert-That ($failure.status.stage -eq 'implement') 'Gate failure did not reopen implementation'

    $blocked = Invoke-Flow 0 submit $id $failure.status.revision block (Join-Path $fixtures 'blocked.md')
    $paused = Invoke-Flow 0 next $id
    Assert-That ($paused.action -eq 'resume' -and $paused.status.blocked) 'Blocked state was not persisted'
    $resumed = Invoke-Flow 0 submit $id $blocked.revision resume (Join-Path $fixtures 'resumed.md')
    $status = Invoke-Flow 0 status $id
    Assert-That ($status.rejected_candidates -eq 1 -and $status.revision -eq $resumed.revision) 'Rejection count was not preserved across resume'
    $handoff = Invoke-Flow 0 next $id
    Assert-That ($handoff.role -eq 'implementer' -and $handoff.gate_failures[0].id -eq 'acceptance') 'Resume lost the gate evidence'

    Copy-Item -LiteralPath (Join-Path $fixtures 'fixed.rs') -Destination $appFile -Force
    Invoke-Git add -- $appFile
    Invoke-Git commit --quiet -m 'Implement SLUG-001 acceptance criteria'
    $candidate = Invoke-Git rev-parse HEAD
    $fixed = Invoke-Flow 0 submit $id $resumed.revision candidate (Join-Path $fixtures 'implementation.md') $candidate
    # A deliberate dirty edit proves the gate uses the pinned commit.
    Set-Content -LiteralPath $appFile -Value 'this should never be compiled'
    $passed = Invoke-Flow 0 gate $id $fixed.revision
    Copy-Item -LiteralPath (Join-Path $fixtures 'fixed.rs') -Destination $appFile -Force
    Assert-That ($passed.status.stage -eq 'review' -and $passed.checks.Count -eq 2) 'Pinned candidate did not pass both gates'
    $review = Invoke-Flow 0 next $id
    Assert-That ($review.status.candidate -eq $candidate -and $review.role -eq 'reviewer') 'Review handoff did not pin the candidate'

    $reviewReport = Join-Path $fixtures 'review.md'
    Invoke-Flow 2 submit $id $passed.status.revision approve $reviewReport $firstCommit
    $done = Invoke-Flow 0 submit $id $passed.status.revision approve $reviewReport $candidate
    $audit = Invoke-Flow 0 audit $id
    $final = Invoke-Flow 0 next $id
    Assert-That ($done.stage -eq 'done' -and $final.action -eq 'report_completion') 'Story did not reach done'
    Assert-That ($audit.events.Count -eq 9 -and $final.status.revision -eq 8) 'Unexpected audit length'
    Assert-That (-not (Invoke-Git status --porcelain)) 'Demo worktree should be clean'

    [pscustomobject] @{
        story = $id
        stage = $done.stage
        revision = $done.revision
        candidate = $candidate
        failed_gate = $failure.checks[-1].id
        passed_checks = @($passed.checks | ForEach-Object id)
        events = @($audit.events | ForEach-Object { $_.body.event.type })
        workspace = $run
        journal = Join-Path $run ".storyflow/$id/events"
    } | ConvertTo-Json
} finally {
    Pop-Location
}
