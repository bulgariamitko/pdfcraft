# Print a print-ready PDF on Windows (issue #756). The job arrives in PDFCRAFT_* environment
# variables, never as arguments: printer names contain spaces and quotes, and the document's
# path must not be visible in the process list while it prints.
#
# The in-box Windows.Data.Pdf renders each sheet at the printer's own resolution and
# System.Drawing.Printing spools it with the driver's settings — copies, collation, duplex and
# colour reach the spooler as the job's, the way `lp -o` sends them on CUPS. No PDF reader is
# needed, and the calling code touches no Win32 API.
#
# A PostScript driver gets the sheets as PostScript instead, when Ghostscript is installed: a
# rendered bitmap is RGB, so the driver turns the document's CMYK into RGB and back and the
# colours come out paler than from Acrobat, which sends PostScript too. Each sheet is turned into
# EPS by Ghostscript (colours as in the document) and passed through the driver
# (POSTSCRIPT_PASSTHROUGH), which still applies its own settings: paper, copies, duplex,
# finishing. Any other driver, or no Ghostscript, keeps the bitmap.
#
# PDFCRAFT_PRINT_DRYRUN=1 renders every sheet to a PNG in a temporary folder and prints
# nothing: the tests run the whole rendering path on machines without a printer, CI included.
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = New-Object System.Text.UTF8Encoding($false)

Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Runtime.WindowsRuntime
try {
    $null = [Windows.Data.Pdf.PdfDocument, Windows.Data.Pdf, ContentType = WindowsRuntime]
} catch {
    throw 'this Windows installation cannot render PDFs: the Windows.Data.Pdf component is missing'
}
$null = [Windows.Storage.StorageFile, Windows.Storage, ContentType = WindowsRuntime]

# Await the WinRT asynchronous APIs from .NET Framework's PowerShell. AsTask is an extension
# method: found by reflecting over the static class — with `.`, an instance call on the type
# object, since `GetMethods` is not static and `::` would not find it.
$type = [System.WindowsRuntimeSystemExtensions]
$opTask = $type.GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
$actTask = $type.GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncAction'
} | Select-Object -First 1
function AwaitOp($operation, $resultType) {
    $task = $opTask.MakeGenericMethod($resultType).Invoke($null, @($operation))
    $task.Wait()
    $task.Result
}
function AwaitAct($action) {
    $task = $actTask.Invoke($null, @($action))
    $task.Wait()
}

# One page rendered the size it will be drawn (`$drawW` × `$drawH`, hundredths of an inch), at
# `$dpi` dots per inch, aspect-fitted. The caller disposes the bitmap.
function Render-Sheet($page, $drawW, $drawH, $dpi) {
    $w = $page.Size.Width
    $h = $page.Size.Height
    $scale = [Math]::Min($drawW / 100.0 * 96.0 / $w, $drawH / 100.0 * 96.0 / $h)
    $options = [Windows.Data.Pdf.PdfPageRenderOptions]::new()
    $options.DestinationWidth = [int][Math]::Ceiling($w * $scale * $dpi / 96.0)
    $options.DestinationHeight = [int][Math]::Ceiling($h * $scale * $dpi / 96.0)
    $stream = [System.IO.MemoryStream]::new()
    try {
        $random = [System.IO.WindowsRuntimeStreamExtensions]::AsRandomAccessStream($stream)
        AwaitAct ($page.RenderToStreamAsync($random, $options))
        $stream.Position = 0
        # Copy out of the stream: GDI+ reads an Image lazily and needs its stream alive, so the
        # render below releases the stream with the function.
        $png = [System.Drawing.Image]::FromStream($stream)
        try { return [System.Drawing.Bitmap]::new($png) } finally { $png.Dispose() }
    } finally {
        $stream.Dispose()
    }
}

$source = AwaitOp ([Windows.Storage.StorageFile]::GetFileFromPathAsync($env:PDFCRAFT_PRINT_FILE)) ([Windows.Storage.StorageFile])
$document = AwaitOp ([Windows.Data.Pdf.PdfDocument]::LoadFromFileAsync($source)) ([Windows.Data.Pdf.PdfDocument])
if ($document.PageCount -lt 1) { throw 'the print-ready PDF has no pages' }

if ($env:PDFCRAFT_PRINT_DRYRUN -eq '1') {
    # Every sheet rendered as the printer would take it, to prove the path without one.
    $folder = Join-Path ([System.IO.Path]::GetTempPath()) ('pdfcraft-dryrun-' + $PID)
    $null = New-Item -ItemType Directory -Force $folder
    for ($i = 0; $i -lt $document.PageCount; $i++) {
        $page = $document.GetPage($i)
        try {
            $bitmap = Render-Sheet $page 750 1000 150
            try { $bitmap.Save((Join-Path $folder "sheet-$i.png"), [System.Drawing.Imaging.ImageFormat]::Png) } finally { $bitmap.Dispose() }
        } finally {
            $page.Dispose()
        }
    }
    Write-Output ('rendered ' + $document.PageCount + ' sheet(s)')
    return
}

$printer = [System.Drawing.Printing.PrintDocument]::new()
$printer.DocumentName = $env:PDFCRAFT_PRINT_TITLE
$settings = $printer.PrinterSettings
if ($env:PDFCRAFT_PRINT_PRINTER) {
    try {
        $settings.PrinterName = $env:PDFCRAFT_PRINT_PRINTER
    } catch {
        throw ('the printer is not available: ' + $env:PDFCRAFT_PRINT_PRINTER)
    }
}
$settings.Copies = [Math]::Min(32767, [Math]::Max(1, [int]$env:PDFCRAFT_PRINT_COPIES))
$settings.Collate = ($env:PDFCRAFT_PRINT_COLLATE -eq '1')
# DEVMODE's names are about the binding edge of a portrait page: Vertical (DMDUP_VERTICAL)
# binds on the long edge, Horizontal (DMDUP_HORIZONTAL) on the short edge.
$settings.Duplex = switch ($env:PDFCRAFT_PRINT_DUPLEX) {
    'long-edge' { [System.Drawing.Printing.Duplex]::Vertical }
    'short-edge' { [System.Drawing.Printing.Duplex]::Horizontal }
    default { [System.Drawing.Printing.Duplex]::Simplex }
}
if ($env:PDFCRAFT_PRINT_GRAYSCALE -eq '1') { $printer.DefaultPageSettings.Color = $false }

# The PostScript route: Ghostscript's newest gswin64c (or PDFCRAFT_GHOSTSCRIPT), a driver that
# takes POSTSCRIPT_PASSTHROUGH, and one EPS per sheet in a private folder. Any failure here
# leaves `$eps` empty and the job prints as bitmaps.
Add-Type @'
using System;
using System.IO;
using System.Runtime.InteropServices;
public static class PdfCraftPostScript {
    const int QUERYESCSUPPORT = 8, POSTSCRIPT_PASSTHROUGH = 4115;
    [DllImport("gdi32.dll", CharSet = CharSet.Unicode)] static extern IntPtr CreateDCW(string driver, string device, string output, IntPtr devmode);
    [DllImport("gdi32.dll")] static extern bool DeleteDC(IntPtr dc);
    [DllImport("gdi32.dll")] static extern int ExtEscape(IntPtr dc, int escape, int inputSize, byte[] input, int outputSize, IntPtr output);
    public static bool Supported(string printer) {
        IntPtr dc = CreateDCW("WINSPOOL", printer, null, IntPtr.Zero);
        if (dc == IntPtr.Zero) return false;
        try { return ExtEscape(dc, QUERYESCSUPPORT, 4, BitConverter.GetBytes(POSTSCRIPT_PASSTHROUGH), 0, IntPtr.Zero) > 0; }
        finally { DeleteDC(dc); }
    }
    // The escape takes a 16-bit byte count before the data, so the file goes in chunks.
    public static void Send(IntPtr dc, byte[] data) {
        for (int at = 0; at < data.Length; at += 32000) {
            int n = Math.Min(32000, data.Length - at);
            byte[] chunk = new byte[n + 2];
            chunk[0] = (byte)(n & 0xff);
            chunk[1] = (byte)(n >> 8);
            Array.Copy(data, at, chunk, 2, n);
            if (ExtEscape(dc, POSTSCRIPT_PASSTHROUGH, chunk.Length, chunk, 0, IntPtr.Zero) <= 0)
                throw new IOException("the driver refused the PostScript");
        }
    }
}
'@
$script:eps = $null
$ghostscript = $env:PDFCRAFT_GHOSTSCRIPT
if (-not $ghostscript) {
    $ghostscript = Get-ChildItem (Join-Path $env:ProgramFiles 'gs') -Directory -ErrorAction SilentlyContinue |
        Sort-Object { try { [version]($_.Name -replace '^gs', '') } catch { [version]'0.0' } } -Descending |
        ForEach-Object { Join-Path $_.FullName 'bin\gswin64c.exe' } | Where-Object { Test-Path $_ } | Select-Object -First 1
}
if ($ghostscript -and (Test-Path $ghostscript) -and [PdfCraftPostScript]::Supported($settings.PrinterName)) {
    $epsFolder = Join-Path ([System.IO.Path]::GetTempPath()) ('pdfcraft-ps-' + [guid]::NewGuid().ToString('N'))
    try {
        $null = New-Item -ItemType Directory -Force $epsFolder
        $arguments = @('-q', '-dNOPAUSE', '-dBATCH', '-dSAFER', '-sDEVICE=eps2write')
        if ($env:PDFCRAFT_PRINT_GRAYSCALE -eq '1') { $arguments += @('-sColorConversionStrategy=Gray', '-dProcessColorModel=/DeviceGray') }
        $arguments += @(('-sOutputFile=' + (Join-Path $epsFolder 'sheet-%d.eps')), $env:PDFCRAFT_PRINT_FILE)
        $null = & $ghostscript @arguments 2>&1
        # An array even for one sheet: PowerShell unwraps a single result to the string itself.
        $files = @(1..$document.PageCount | ForEach-Object { Join-Path $epsFolder "sheet-$_.eps" })
        if ($LASTEXITCODE -eq 0 -and -not ($files | Where-Object { -not (Test-Path $_) })) { $script:eps = $files }
    } catch {
        $script:eps = $null
    }
    if (-not $script:eps) { Remove-Item -Recurse -Force $epsFolder -ErrorAction SilentlyContinue }
}
$invariant = [System.Globalization.CultureInfo]::InvariantCulture

# The sheets are already laid out at their final size (CUPS gets `fit-to-page=false`), so each
# one is printed at 100% on paper of its own size and orientation. Before every sheet the driver
# is asked for that paper — the size closest to the sheet's, within an eighth of an inch, turned
# either way — and turned landscape for a wide sheet; for a size it doesn't list, a custom paper
# of the sheet's size (a driver without custom sizes keeps its own paper).
$paperSizes = @($settings.PaperSizes)
$script:sheets = 0
$printer.add_QueryPageSettings({
    param($sender, $e)
    if ($script:sheets -ge $document.PageCount) { return }
    $page = $document.GetPage($script:sheets)
    try {
        # Windows.Data.Pdf measures in 1/96 inch, PageSettings in 1/100 inch.
        $w = $page.Size.Width / 96.0 * 100.0
        $h = $page.Size.Height / 96.0 * 100.0
    } finally {
        $page.Dispose()
    }
    $long = [Math]::Max($w, $h)
    $short = [Math]::Min($w, $h)
    $best = $null
    $bestOff = 12.5
    foreach ($paper in $paperSizes) {
        $off = [Math]::Abs([Math]::Max($paper.Width, $paper.Height) - $long) + [Math]::Abs([Math]::Min($paper.Width, $paper.Height) - $short)
        if ($off -lt $bestOff) { $best = $paper; $bestOff = $off }
    }
    if (-not $best) { $best = [System.Drawing.Printing.PaperSize]::new('Custom', [int][Math]::Round($short), [int][Math]::Round($long)) }
    $e.PageSettings.PaperSize = $best
    $e.PageSettings.Landscape = ($w -gt $h)
})

# One sheet at a time: its page is rendered when the driver asks for it, so a long document
# holds only one page's bitmap in memory.
$printer.add_PrintPage({
    param($sender, $e)
    if ($script:sheets -ge $document.PageCount) { $e.HasMorePages = $false; return }
    $page = $document.GetPage($script:sheets)
    $bitmap = $null
    try {
        # Hundredths of an inch throughout. The sheet is drawn at 100%, centred on the whole
        # page — not its 1-inch default margins — and shrunk only when the paper is smaller than
        # the sheet. The drawing origin is the printable area's corner, so the page's own corner
        # is the hard margin back from it; what falls outside the printable area is clipped, as
        # on CUPS.
        $paper = $e.PageBounds
        $w = $page.Size.Width / 96.0 * 100.0
        $h = $page.Size.Height / 96.0 * 100.0
        $scale = [Math]::Min(1.0, [Math]::Min($paper.Width / $w, $paper.Height / $h))
        $drawW = $w * $scale
        $drawH = $h * $scale
        $left = ($paper.Width - $drawW) / 2.0 - $e.PageSettings.HardMarginX
        $top = ($paper.Height - $drawH) / 2.0 - $e.PageSettings.HardMarginY
        if ($script:eps) {
            # The driver's user space here is GDI's: device pixels from the printable area's
            # corner, y down. The EPS is in points from the sheet's lower-left corner, so: to the
            # box's lower-left, y up, points at the scale, clipped to the sheet.
            $dpiX = $e.Graphics.DpiX
            $dpiY = $e.Graphics.DpiY
            $wide = $page.Size.Width / 96.0 * 72.0
            $high = $page.Size.Height / 96.0 * 72.0
            $place = [string]::Format($invariant, '{0:0.###} {1:0.###} translate {2:0.######} {3:0.######} scale 0 0 moveto {4:0.###} 0 lineto {4:0.###} {5:0.###} lineto 0 {5:0.###} lineto closepath clip newpath',
                $left * $dpiX / 100.0, ($top + $drawH) * $dpiY / 100.0, $scale * $dpiX / 72.0, -$scale * $dpiY / 72.0, $wide, $high)
            # The EPS runs on a dictionary stack of only the permanent dictionaries: the driver's
            # own (Pscript_WinNT_Compat's short names among them) are set aside and put back
            # after, since Ghostscript's procedures look some of the same names up.
            $head = "`nuserdict /PdfCraftSheet save put count userdict /PdfCraftOps 3 -1 roll put`n" +
                "userdict /PdfCraftBase /globaldict where {pop 3} {2} ifelse put userdict /PdfCraftStack countdictstack array dictstack put`n" +
                "countdictstack PdfCraftBase sub {end} repeat userdict begin /showpage {} def`n" +
                "0 setgray 0 setlinecap 1 setlinewidth 0 setlinejoin 10 setmiterlimit [] 0 setdash newpath`n" +
                "$place`n%%BeginDocument: sheet-$($script:sheets + 1).eps`n"
            $tail = "`n%%EndDocument`ncount PdfCraftOps sub {pop} repeat countdictstack PdfCraftBase sub {end} repeat`n" +
                "PdfCraftStack PdfCraftBase 1 index length PdfCraftBase sub getinterval {begin} forall PdfCraftSheet restore`n"
            $hdc = $e.Graphics.GetHdc()
            try {
                [PdfCraftPostScript]::Send($hdc, [System.Text.Encoding]::ASCII.GetBytes($head))
                [PdfCraftPostScript]::Send($hdc, [System.IO.File]::ReadAllBytes($script:eps[$script:sheets]))
                [PdfCraftPostScript]::Send($hdc, [System.Text.Encoding]::ASCII.GetBytes($tail))
            } finally {
                $e.Graphics.ReleaseHdc($hdc)
            }
        } else {
            # The printer's resolution, at most 600 dpi and at most ~64 megapixels a sheet, so a
            # poster-sized sheet can't run the machine out of memory.
            $dpi = [Math]::Min(600, [Math]::Max(72, $e.Graphics.DpiX))
            $dpi = [Math]::Max(36, [Math]::Min($dpi, [Math]::Sqrt(64e6 / ($drawW / 100.0 * $drawH / 100.0))))
            $bitmap = Render-Sheet $page $drawW $drawH $dpi
            $box = [System.Drawing.RectangleF]::new($left, $top, $drawW, $drawH)
            $e.Graphics.DrawImage($bitmap, $box)
        }
    } finally {
        if ($bitmap) { $bitmap.Dispose() }
        $page.Dispose()
    }
    $script:sheets++
    $e.HasMorePages = ($script:sheets -lt $document.PageCount)
})
try {
    $printer.Print()
} finally {
    if ($script:eps) { Remove-Item -Recurse -Force $epsFolder -ErrorAction SilentlyContinue }
}
Write-Output ('spooled ' + $document.PageCount + ' sheet(s)')
