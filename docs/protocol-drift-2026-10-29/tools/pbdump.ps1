param([string]$File, [int]$MaxDepth = 3)
# Minimal protobuf wire-format dumper (read-only). Prints field number, wire type, length and a preview.
$c = Get-Content -LiteralPath $File -Raw
$hex = ([regex]::Match($c, 'raw_hex=([0-9a-fA-F]+)')).Groups[1].Value
$b = [byte[]]::new($hex.Length / 2)
for ($i = 0; $i -lt $b.Length; $i++) { $b[$i] = [Convert]::ToByte($hex.Substring($i * 2, 2), 16) }

function ReadVarint([byte[]]$buf, [ref]$pos, [int]$end) {
  [uint64]$r = 0; $shift = 0
  while ($pos.Value -lt $end) {
    $x = $buf[$pos.Value]; $pos.Value++
    $r = $r -bor ([uint64]($x -band 0x7f) -shl $shift)
    if (($x -band 0x80) -eq 0) { return $r }
    $shift += 7; if ($shift -gt 63) { throw 'varint too long' }
  }
  throw 'truncated varint'
}

function Preview([byte[]]$buf, [int]$s, [int]$n) {
  $k = [Math]::Min($n, 48)
  $seg = $buf[$s..($s + $k - 1)]
  if ($n -eq 0) { return '' }
  $printable = ($seg | Where-Object { $_ -lt 32 -or $_ -gt 126 }).Count -eq 0
  if ($printable) { return '"' + [Text.Encoding]::ASCII.GetString($seg) + '"' }
  return ($seg | ForEach-Object { $_.ToString('x2') }) -join ' '
}

function TryParse([byte[]]$buf, [int]$s, [int]$e) {
  $p = $s
  try {
    while ($p -lt $e) {
      $tag = ReadVarint $buf ([ref]$p) $e; $wt = [int]($tag -band 7); $fn = $tag -shr 3
      if ($fn -eq 0) { return $false }
      switch ($wt) {
        0 { [void](ReadVarint $buf ([ref]$p) $e) }
        1 { $p += 8 }
        2 { $l = [int](ReadVarint $buf ([ref]$p) $e); $p += $l }
        5 { $p += 4 }
        default { return $false }
      }
      if ($p -gt $e) { return $false }
    }
    return $true
  } catch { return $false }
}

function Dump([byte[]]$buf, [int]$s, [int]$e, [int]$depth, [string]$path) {
  $p = $s
  while ($p -lt $e) {
    $at = $p - $s
    try { $tag = ReadVarint $buf ([ref]$p) $e } catch { "{0}{1} @{2}: <bad tag>" -f ('  ' * $depth), $path, $at; return }
    $wt = [int]($tag -band 7); $fn = $tag -shr 3
    $ind = '  ' * $depth
    switch ($wt) {
      0 { $v = ReadVarint $buf ([ref]$p) $e; "{0}{1}.{2} @{3} varint = {4}" -f $ind, $path, $fn, $at, $v }
      1 { "{0}{1}.{2} @{3} fixed64" -f $ind, $path, $fn, $at; $p += 8 }
      5 { "{0}{1}.{2} @{3} fixed32" -f $ind, $path, $fn, $at; $p += 4 }
      2 {
        $l = [int](ReadVarint $buf ([ref]$p) $e)
        "{0}{1}.{2} @{3} len={4}  {5}" -f $ind, $path, $fn, $at, $l, (Preview $buf $p $l)
        if ($depth -lt $MaxDepth -and $l -gt 0 -and (TryParse $buf $p ($p + $l))) {
          Dump $buf $p ($p + $l) ($depth + 1) "$path.$fn"
        }
        $p += $l
      }
      default { "{0}{1}.{2} @{3} INVALID wire type {4} (raw byte 0x{5:x2} '{6}')" -f $ind, $path, $fn, $at, $wt, $buf[$p - 1], [char]$buf[$p - 1]; return }
    }
  }
}

"file: $File  bytes: $($b.Length)"
Dump $b 0 $b.Length 0 'msg'
