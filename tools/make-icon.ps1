# Generates DualMix\Assets\dualmix.ico (multi-size, PNG-compressed entries).
# Run with Windows PowerShell 5.1 (needs full System.Drawing):
#   powershell -ExecutionPolicy Bypass -File tools\make-icon.ps1
param([string]$Out = (Join-Path $PSScriptRoot '..\DualMix\Assets\dualmix.ico'))

Add-Type -AssemblyName System.Drawing

function New-IconBitmap([int]$size) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([System.Drawing.Color]::Transparent)

    $pad = [Math]::Max(1, [int]($size * 0.04))
    $rect = New-Object System.Drawing.RectangleF $pad, $pad, ($size - 2 * $pad), ($size - 2 * $pad)

    # Rounded dark background
    $r = $rect.Width * 0.24
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($rect.X, $rect.Y, 2 * $r, 2 * $r, 180, 90)
    $path.AddArc($rect.Right - 2 * $r, $rect.Y, 2 * $r, 2 * $r, 270, 90)
    $path.AddArc($rect.Right - 2 * $r, $rect.Bottom - 2 * $r, 2 * $r, 2 * $r, 0, 90)
    $path.AddArc($rect.X, $rect.Bottom - 2 * $r, 2 * $r, 2 * $r, 90, 90)
    $path.CloseFigure()
    $bg = New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::FromArgb(255, 23, 26, 33))
    $g.FillPath($bg, $path)

    # Two bars: orange (game) and blue (chat), plus a white balance knob between them
    $orange = [System.Drawing.Color]::FromArgb(255, 255, 107, 61)
    $blue = [System.Drawing.Color]::FromArgb(255, 61, 169, 255)
    $barW = $rect.Width * 0.2
    $baseY = $rect.Bottom - $rect.Height * 0.2
    $leftX = $rect.X + $rect.Width * 0.2
    $rightX = $rect.Right - $rect.Width * 0.2 - $barW
    $hL = $rect.Height * 0.55
    $hR = $rect.Height * 0.38
    $g.FillRectangle((New-Object System.Drawing.SolidBrush $orange), [single]$leftX, [single]($baseY - $hL), [single]$barW, [single]$hL)
    $g.FillRectangle((New-Object System.Drawing.SolidBrush $blue), [single]$rightX, [single]($baseY - $hR), [single]$barW, [single]$hR)

    # balance line + knob
    $lineY = $rect.Y + $rect.Height * 0.2
    $penW = [Math]::Max(1.0, $size * 0.06)
    $pen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(255, 232, 234, 240)), $penW
    $pen.StartCap = 'Round'; $pen.EndCap = 'Round'
    $g.DrawLine($pen, [single]($leftX), [single]$lineY, [single]($rightX + $barW), [single]$lineY)
    $kr = $size * 0.11
    $kx = $rect.X + $rect.Width * 0.42
    $g.FillEllipse((New-Object System.Drawing.SolidBrush ([System.Drawing.Color]::White)), [single]($kx - $kr), [single]($lineY - $kr), [single](2 * $kr), [single](2 * $kr))

    $g.Dispose()
    return $bmp
}

$sizes = 16, 20, 24, 32, 40, 48, 64, 128, 256
$pngs = @()
foreach ($s in $sizes) {
    $bmp = New-IconBitmap $s
    $ms = New-Object System.IO.MemoryStream
    $bmp.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $pngs += , $ms.ToArray()
    $bmp.Dispose()
}

$dir = Split-Path $Out -Parent
if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force $dir | Out-Null }
$fs = [System.IO.File]::Create($Out)
$bw = New-Object System.IO.BinaryWriter $fs
$bw.Write([UInt16]0); $bw.Write([UInt16]1); $bw.Write([UInt16]$sizes.Count)
$offset = 6 + 16 * $sizes.Count
for ($i = 0; $i -lt $sizes.Count; $i++) {
    $s = $sizes[$i]; $len = $pngs[$i].Length
    $dim = if ($s -ge 256) { 0 } else { $s }
    $bw.Write([byte]$dim); $bw.Write([byte]$dim); $bw.Write([byte]0); $bw.Write([byte]0)
    $bw.Write([UInt16]1); $bw.Write([UInt16]32); $bw.Write([UInt32]$len); $bw.Write([UInt32]$offset)
    $offset += $len
}
foreach ($p in $pngs) { $bw.Write($p) }
$bw.Close()
Write-Host "Icon written: $Out"
