# Dump Cursor agent-harness protobuf schema with message/enum references resolved to names.
# Read-only. Output line format:
#   message Name { <num> <field> <type>[?|*|map<k,v>] [oneof X] ; ... }
#   enum Name { 0 UNSPECIFIED; 1 FOO; ... }
# Usage:
#   .\resolve-harness-schema.ps1 -Out schema.resolved.txt
#   .\resolve-harness-schema.ps1 -Root RunInferenceClientMessage,RunInferenceServerMessage   # print closure only
param(
  [string]$Generation,
  [string]$Out,
  [string[]]$Root
)
$base = Join-Path $env:APPDATA 'Cursor\agent-harness'
if (-not $Generation) { $Generation = (Get-Content (Join-Path $base 'current') -Raw).Trim() }
$t = [IO.File]::ReadAllText((Join-Path $base "versions\$Generation\agent-harness.cjs"))

$scalar = @{ '1'='double'; '2'='float'; '3'='int64'; '4'='uint64'; '5'='int32'; '6'='fixed64'; '7'='fixed32';
  '8'='bool'; '9'='string'; '12'='bytes'; '13'='uint32'; '15'='sfixed32'; '16'='sfixed64'; '17'='sint32'; '18'='sint64' }

# Minified identifiers are reused across module scopes, so keep every definition with its offset
# and resolve a reference to the definition closest to the referencing message.
$defs = @{}   # identifier -> list of @{ at; name }
function AddDef($id, $at, $name) { if (-not $defs.ContainsKey($id)) { $defs[$id] = New-Object System.Collections.ArrayList }; [void]$defs[$id].Add(@{ at = $at; name = $name }) }
$enumVals = @{}
foreach ($m in [regex]::Matches($t, '([\w$]+)=Re\([\w$]+,[\w$]+,"([A-Za-z0-9_.]+)",(\[(?:\[\d+,"[^"]*"\],?)*\])')) {
  AddDef $m.Groups[1].Value $m.Index $m.Groups[2].Value
  $enumVals[$m.Groups[2].Value] = ([regex]::Matches($m.Groups[3].Value, '\[(\d+),"([^"]*)"\]') | ForEach-Object { "$($_.Groups[1].Value) $($_.Groups[2].Value)" }) -join '; '
}
$desc = @{}
$rx = [regex]'([\w$]+)=class [\w$]+ extends [\w$.]+\{'
foreach ($m in $rx.Matches($t)) {
  $win = $t.Substring($m.Index, [Math]::Min(6000, $t.Length - $m.Index))
  $s = [regex]::Match($win, 'static \$\(\)\{return\["((?:[^"\\]|\\.)*)"((?:,[\w$]+)*)\]')
  if (-not $s.Success) { continue }
  $d = $s.Groups[1].Value; $name = $d.Split('|')[0]
  AddDef $m.Groups[1].Value $m.Index $name
  if (-not $desc.ContainsKey($name)) { $desc[$name] = @{ d = $d; at = $m.Index; refs = @($s.Groups[2].Value.TrimStart(',') -split ',' | Where-Object { $_ }) } }
}

function RefName($id, $at) {
  if (-not $defs.ContainsKey($id)) { return "?$id" }
  ($defs[$id] | Sort-Object { [Math]::Abs($_.at - $at) } | Select-Object -First 1).name
}
function ResolveType([string]$x, [string[]]$refs, [System.Collections.Generic.List[string]]$deps, [int]$at) {
  if ($x -match '^#(\d+)$') { $r = RefName $refs[[int]$Matches[1]] $at; $deps.Add($r); return $r }
  if ($scalar.ContainsKey($x)) { return $scalar[$x] }
  return $x
}
function Render($name) {
  $e = $desc[$name]; if (-not $e) { return $null }
  $parts = $e.d.Split('|'); $fields = @()
  $deps = New-Object System.Collections.Generic.List[string]
  $refs = [string[]]$e.refs
  foreach ($p in $parts[1..($parts.Length - 1)]) {
    if (-not $p) { continue }
    $tok = $p.Split(' ')
    $num = $tok[0]; $fname = $tok[1]; $ty = $tok[2]; $oneof = if ($tok.Length -gt 3) { " oneof $($tok[3])" } else { '' }
    $suffix = ''
    if ($ty -match '[?*]$') { $suffix = $ty.Substring($ty.Length - 1); $ty = $ty.Substring(0, $ty.Length - 1) }
    if ($ty -match ',') { $kv = $ty.Split(','); $tyName = "map<$(ResolveType $kv[0] $refs $deps $e.at),$(ResolveType $kv[1] $refs $deps $e.at)>" }
    else { $tyName = ResolveType $ty $refs $deps $e.at }
    $fields += "$num $fname $tyName$suffix$oneof"
  }
  [pscustomobject]@{ text = "message $name { " + ($fields -join '; ') + ' }'; deps = $deps }
}

$lines = New-Object System.Collections.Generic.List[string]
if ($Root) {
  $seen = @{}; $queue = New-Object System.Collections.Queue; $Root | ForEach-Object { $queue.Enqueue($_) }
  while ($queue.Count) {
    $n = $queue.Dequeue(); if ($seen[$n]) { continue }; $seen[$n] = $true
    if ($enumVals.ContainsKey($n)) { $lines.Add("enum $n { $($enumVals[$n]) }"); continue }
    $r = Render $n
    if (-not $r) { $lines.Add("// missing $n"); continue }
    $lines.Add($r.text); $r.deps | Where-Object { -not $seen[$_] } | ForEach-Object { $queue.Enqueue($_) }
  }
} else {
  foreach ($n in ($desc.Keys | Sort-Object)) { $lines.Add((Render $n).text) }
  foreach ($n in ($enumVals.Keys | Sort-Object)) { $lines.Add("enum $n { $($enumVals[$n]) }") }
}
if ($Out) { $lines | Set-Content $Out -Encoding UTF8; "wrote $Out ($($lines.Count) lines; messages=$($desc.Count) enums=$($enumVals.Count))" } else { $lines }
