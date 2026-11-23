# Generate the NexusRAR app icon set using .NET System.Drawing.
#
# Design (Sprint 5.7.31):
#   - 512x512 rounded square base with cyan-to-blue gradient
#   - 3 horizontal "compressed data" bars in the centre
#   - small chevron-down at the top-right (compression indicator)
#   - subtle inner highlight (1 px white at 18 % alpha) for depth
#
# Output:
#   icons/icon.png          512x512 (master)
#   icons/128x128@2x.png    256x256  (HiDPI 128)
#   icons/128x128.png       128x128
#   icons/32x32.png          32x32
#   icons/icon.ico          multi-resolution .ico (16/32/48/64/128/256)

Add-Type -AssemblyName System.Drawing

$root = Split-Path -Parent $MyInvocation.MyCommand.Path
$masterPath = Join-Path $root "icon.png"

function Draw-Logo([System.Drawing.Bitmap]$bmp) {
    $g = [System.Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = [System.Drawing.Drawing2D.SmoothingMode]::AntiAlias
    $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
    $g.PixelOffsetMode = [System.Drawing.Drawing2D.PixelOffsetMode]::HighQuality
    $g.Clear([System.Drawing.Color]::Transparent)

    $sz = $bmp.Width
    # Scale-invariant coordinates (designed at 512).
    $u = $sz / 512.0

    # --- Outer rounded square background with cyan-to-blue gradient ---
    $radius = [int](96 * $u)
    $rect = New-Object System.Drawing.Rectangle 0, 0, ($sz - 1), ($sz - 1)
    $path = New-Object System.Drawing.Drawing2D.GraphicsPath
    $path.AddArc($rect.X, $rect.Y, $radius * 2, $radius * 2, 180, 90)
    $path.AddArc($rect.Right - $radius * 2, $rect.Y, $radius * 2, $radius * 2, 270, 90)
    $path.AddArc($rect.Right - $radius * 2, $rect.Bottom - $radius * 2, $radius * 2, $radius * 2, 0, 90)
    $path.AddArc($rect.X, $rect.Bottom - $radius * 2, $radius * 2, $radius * 2, 90, 90)
    $path.CloseFigure()

    $bgRect = New-Object System.Drawing.RectangleF 0, 0, $sz, $sz
    $bgGrad = New-Object System.Drawing.Drawing2D.LinearGradientBrush(
        $bgRect,
        [System.Drawing.Color]::FromArgb(255, 8, 145, 178),    # cyan-600-ish
        [System.Drawing.Color]::FromArgb(255, 29, 78, 216),    # blue-700-ish
        [System.Drawing.Drawing2D.LinearGradientMode]::ForwardDiagonal
    )
    $g.FillPath($bgGrad, $path)
    $bgGrad.Dispose()

    # --- Subtle inner border highlight (top half) ---
    $borderPen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(60, 255, 255, 255)), ([single](1.5 * $u))
    $g.DrawPath($borderPen, $path)
    $borderPen.Dispose()

    # --- 3 horizontal "compressed data" bars (centred) ---
    # Designed at 512: bars at y=176, 240, 304; width=280; height=40; gap=26.
    $barColor = [System.Drawing.Color]::FromArgb(255, 236, 254, 255)  # near-white with cyan tint
    $barRadius = [int](18 * $u)
    $barW = [int](280 * $u)
    $barH = [int](40 * $u)
    $barX = [int](($sz - $barW) / 2)
    $barYs = @( [int](170 * $u), [int](236 * $u), [int](302 * $u) )

    foreach ($barY in $barYs) {
        $barRect = New-Object System.Drawing.Rectangle $barX, $barY, $barW, $barH
        $barPath = New-Object System.Drawing.Drawing2D.GraphicsPath
        $r = $barRadius
        $barPath.AddArc($barRect.X, $barRect.Y, $r * 2, $r * 2, 180, 90)
        $barPath.AddArc($barRect.Right - $r * 2, $barRect.Y, $r * 2, $r * 2, 270, 90)
        $barPath.AddArc($barRect.Right - $r * 2, $barRect.Bottom - $r * 2, $r * 2, $r * 2, 0, 90)
        $barPath.AddArc($barRect.X, $barRect.Bottom - $r * 2, $r * 2, $r * 2, 90, 90)
        $barPath.CloseFigure()
        $g.FillPath((New-Object System.Drawing.SolidBrush $barColor), $barPath)
        $barPath.Dispose()
    }

    # --- Cyan accent glow line above bars (compression wave) ---
    $glowPen = New-Object System.Drawing.Pen ([System.Drawing.Color]::FromArgb(180, 34, 211, 238)), ([single](3 * $u))
    $yGlow = [int](130 * $u)
    $g.DrawLine($glowPen, [int](140 * $u), $yGlow, [int](372 * $u), $yGlow)
    $glowPen.Dispose()

    # --- Small chevron-down at top-right (compression indicator) ---
    $chevColor = [System.Drawing.Color]::FromArgb(220, 255, 255, 255)
    $chevPen = New-Object System.Drawing.Pen $chevColor, ([single](5 * $u))
    $chevPen.StartCap = [System.Drawing.Drawing2D.LineCap]::Round
    $chevPen.EndCap   = [System.Drawing.Drawing2D.LineCap]::Round
    $cx = [int](380 * $u); $cy = [int](100 * $u)
    $g.DrawLine($chevPen, $cx - [int](16 * $u), $cy - [int](10 * $u), $cx, $cy + [int](10 * $u))
    $g.DrawLine($chevPen, $cx, $cy + [int](10 * $u), $cx + [int](16 * $u), $cy - [int](10 * $u))
    $chevPen.Dispose()

    $g.Dispose()
    $path.Dispose()
}

function Save-Png([int]$size, [string]$path) {
    $bmp = New-Object System.Drawing.Bitmap $size, $size, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    Draw-Logo -bmp $bmp
    $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
    $bmp.Dispose()
}

# 1. Master 512x512.
Save-Png 512 $masterPath
"OK master 512"

# 2. 256x256 (used as 128x128@2x -- HiDPI variant).
$path256 = Join-Path $root "128x128@2x.png"
Save-Png 256 $path256
"OK 256"

# 3. 128x128.
$path128 = Join-Path $root "128x128.png"
Save-Png 128 $path128
"OK 128"

# 4. 32x32.
$path32 = Join-Path $root "32x32.png"
Save-Png 32 $path32
"OK 32"

# 5. ICO bundle (multi-resolution).
$icoPath = Join-Path $root "icon.ico"
$sizes = @(16, 32, 48, 64, 128, 256)
$bitmaps = @()
foreach ($s in $sizes) {
    $b = New-Object System.Drawing.Bitmap $s, $s, ([System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
    Draw-Logo -bmp $b
    $bitmaps += ,@{ size = $s; bmp = $b }
}
$out = [System.IO.File]::Create($icoPath)
$bw = New-Object System.IO.BinaryWriter $out
$bw.Write([uint16]0)               # reserved
$bw.Write([uint16]1)               # type = ICO
$bw.Write([uint16]$bitmaps.Count)  # count
$entryOffset = 6 + 16 * $bitmaps.Count
$payloads = @()
foreach ($pair in $bitmaps) {
    $s = $pair.size; $b = $pair.bmp
    $ms = New-Object System.IO.MemoryStream
    $b.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png)
    $ms.Position = 0
    $payloads += ,$ms
}
for ($i = 0; $i -lt $bitmaps.Count; $i++) {
    $s = $bitmaps[$i].size
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))
    $bw.Write([byte]($(if ($s -ge 256) { 0 } else { $s })))
    $bw.Write([byte]0)
    $bw.Write([byte]0)
    $bw.Write([uint16]1)
    $bw.Write([uint16]32)
    $bw.Write([uint32]$payloads[$i].Length)
    $bw.Write([uint32]$entryOffset)
    $entryOffset += [int]$payloads[$i].Length
}
foreach ($ms in $payloads) {
    $ms.Position = 0
    $bw.Write($ms.ToArray())
    $ms.Dispose()
}
$bw.Flush(); $bw.Dispose(); $out.Dispose()
foreach ($pair in $bitmaps) { $pair.bmp.Dispose() }
"OK ico"

"Done. Icons regenerated."