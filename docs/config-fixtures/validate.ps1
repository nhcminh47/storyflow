$ErrorActionPreference = 'Stop'
$schema = Join-Path (Split-Path $PSScriptRoot) 'config.schema.json'
$cases = Get-Content -Raw (Join-Path $PSScriptRoot 'cases.json') | ConvertFrom-Json -AsHashtable
$names = [System.Collections.Generic.HashSet[string]]::new()
$counts = @{ parser = 0; accepted = 0; rejected = 0; pending = 0; pendingDocuments = 0 }

foreach ($case in $cases) {
    if (!$case.name -or !$names.Add($case.name) -or !$case.inputs -or
        $case.expected.outcome -notin @('accept', 'reject') -or
        ($case.expected.outcome -eq 'reject' -and !$case.expected.diagnostic)) {
        throw "Invalid fixture metadata for '$($case.name)'"
    }

    switch ($case.phase) {
        'parser' {
            if ($case.expected.diagnostic -ne 'invalid_json' -or
                (Test-Json -Json $case.inputs.raw -ErrorAction SilentlyContinue) -ne $false) {
                throw "Expected JSON parser rejection for '$($case.name)'"
            }
            $counts.parser++
        }
        { $_ -in @('schema', 'resolver-pending') } {
            if ($case.phase -eq 'resolver-pending') {
                if (!$case.expected.detail) { throw "Missing resolver expectation for '$($case.name)'" }
                $counts.pending++
            } elseif ($case.inputs.documents.Count -ne 1) {
                throw "Expected exactly one schema input for '$($case.name)'"
            }

            foreach ($document in $case.inputs.documents) {
                if ($case.phase -eq 'resolver-pending' -and $document.version -eq 1) { continue }
                $expectValid = $case.phase -eq 'resolver-pending' -or $case.expected.outcome -eq 'accept'
                $json = ConvertTo-Json -InputObject $document -Depth 30 -Compress
                $onError = if ($expectValid) { 'Stop' } else { 'SilentlyContinue' }
                $valid = Test-Json -Json $json -SchemaFile $schema -ErrorAction $onError
                if ($valid -ne $expectValid) { throw "Unexpected schema result for '$($case.name)'" }
                if ($case.phase -eq 'resolver-pending') { $counts.pendingDocuments++ }
                elseif ($valid) { $counts.accepted++ }
                else { $counts.rejected++ }
            }
        }
        default { throw "Unknown validation phase '$($case.phase)'" }
    }
}

"Schema: $($counts.accepted) accepted, $($counts.rejected) rejected. Parser: $($counts.parser) rejected."
"Resolver: $($counts.pending) pending scenarios, $($counts.pendingDocuments) v2 input documents schema-checked."
