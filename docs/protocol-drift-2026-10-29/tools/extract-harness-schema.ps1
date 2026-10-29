# Extract the numbered protobuf schema from Cursor's server-delivered agent-harness module.
# Read-only. Each line: "MessageName|<num> <name> <type>[?|*]|..."
#   type: 8=bool 9=string 12=bytes 13=uint32 3/4=int64/uint64 5=int32 1=double  #N = Nth referenced message/enum
#   "?" optional, "*" repeated, "k,v" map, trailing word = oneof name
#
# Usage:
#   .\extract-harness-schema.ps1                       # current generation -> .\schema\harness-<gen8>.schema.txt
#   .\extract-harness-schema.ps1 -Diff 57c5831e        # also diff against an older dump
param(
  [string]$Generation,
  [string]$OutDir = (Join-Path $PSScriptRoot '..\schema'),
  [string]$Diff
)
$root = Join-Path $env:APPDATA 'Cursor\agent-harness'
if (-not $Generation) { $Generation = (Get-Content (Join-Path $root 'current') -Raw).Trim() }
$cjs = Join-Path $root "versions\$Generation\agent-harness.cjs"
if (-not (Test-Path $cjs)) { throw "missing $cjs" }
$manifest = Get-Content (Join-Path $root "versions\$Generation\manifest.json") -Raw | ConvertFrom-Json
$t = [IO.File]::ReadAllText($cjs)
$lines = [regex]::Matches($t, 'static \$\(\)\{return\["((?:[^"\\]|\\.)*)"') |
  ForEach-Object { $_.Groups[1].Value } | Sort-Object -Unique
New-Item -ItemType Directory -Force $OutDir | Out-Null
$short = $Generation.Substring(0, 8)
$out = Join-Path $OutDir "harness-$short.schema.txt"
$lines | Set-Content $out -Encoding UTF8
"generation=$Generation mainVersion=$($manifest.mainVersion) builtAt=$($manifest.builtAt) messages=$($lines.Count)"
"wrote $out"

if ($Diff) {
  function Load($f) { $h = @{}; Get-Content $f | ForEach-Object { $h[$_.Split('|')[0]] = $_ }; $h }
  $old = Load (Join-Path $OutDir "harness-$Diff.schema.txt")
  $new = Load $out
  '=== removed ==='; $old.Keys | Where-Object { -not $new.ContainsKey($_) } | Sort-Object
  '=== added ===';   $new.Keys | Where-Object { -not $old.ContainsKey($_) } | Sort-Object
  '=== changed ==='
  foreach ($k in ($old.Keys | Where-Object { $new.ContainsKey($_) } | Sort-Object)) {
    if ($old[$k] -ne $new[$k]) {
      $o = $old[$k].Split('|'); $n = $new[$k].Split('|')
      "## $k"
      '  - ' + (($o | Where-Object { $n -notcontains $_ }) -join ' | ')
      '  + ' + (($n | Where-Object { $o -notcontains $_ }) -join ' | ')
    }
  }
}
