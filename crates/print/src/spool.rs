//! The system print spooler. On macOS and Linux this is CUPS: printers come from `lpstat`, jobs
//! are piped to `lp` with the job options (copies, collation, duplex, colour). On Windows the
//! printers come from the spooler (`InstalledPrinters`) and the sheets are rasterized and drawn through .NET's `PrintDocument`
//! (see `WIN_PRINT_SCRIPT`), which works with every driver, not only PDF-capable ones. Other
//! platforms report that printing isn't available yet; the print-ready PDF can still be saved.

use crate::PrintError;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Printer {
    pub name: String,
    pub default: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Duplex {
    #[default]
    Off,
    LongEdge,
    ShortEdge,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    /// `None` = the system default printer.
    pub printer: Option<String>,
    pub copies: u32,
    pub collate: bool,
    pub duplex: Duplex,
    pub grayscale: bool,
    pub title: String,
    /// The printer driver's own job options (a PPD keyword and the chosen value), such as the
    /// paper tray or paper type; see [`printer_options`]. CUPS only.
    pub options: Vec<(String, String)>,
}

impl Default for Job {
    fn default() -> Self {
        Job { printer: None, copies: 1, collate: true, duplex: Duplex::Off, grayscale: false, title: "PdfCraft".into(), options: Vec::new() }
    }
}

/// Parse `lpstat -p -d` output.
pub fn parse_lpstat(out: &str) -> Vec<Printer> {
    let default = out.lines().find_map(|l| l.strip_prefix("system default destination:")).map(|s| s.trim().to_string());
    out.lines()
        .filter_map(|l| l.strip_prefix("printer "))
        .filter_map(|l| l.split_whitespace().next())
        .map(|n| Printer { name: n.to_string(), default: default.as_deref() == Some(n) })
        .collect()
}

/// The `lp` arguments for a job. There is no file argument: `lp` reads the job from stdin.
pub fn lp_args(job: &Job) -> Vec<String> {
    let mut a = Vec::new();
    if let Some(p) = &job.printer {
        a.extend(["-d".to_string(), p.clone()]);
    }
    a.extend(["-n".to_string(), job.copies.clamp(1, 999).to_string()]);
    a.extend(["-t".to_string(), job.title.clone()]);
    let mut opt = |o: &str| a.extend(["-o".to_string(), o.to_string()]);
    opt(if job.collate { "collate=true" } else { "collate=false" });
    opt(match job.duplex {
        Duplex::Off => "sides=one-sided",
        Duplex::LongEdge => "sides=two-sided-long-edge",
        Duplex::ShortEdge => "sides=two-sided-short-edge",
    });
    if job.grayscale {
        opt("print-color-mode=monochrome");
    }
    // The sheets are already laid out at their final size.
    opt("fit-to-page=false");
    for (key, value) in &job.options {
        // Driver options are PPD keywords; anything else is not something lp can be given.
        if is_ppd_keyword(key) && is_ppd_keyword(value) && !HANDLED_OPTIONS.contains(&key.as_str()) {
            a.extend(["-o".to_string(), format!("{key}={value}")]);
        }
    }
    a
}

/// One of the printer driver's job options (a PPD `OpenUI` entry), such as the paper tray.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrinterOption {
    /// The PPD keyword (`InputSlot`).
    pub key: String,
    /// What the driver calls it (`Paper tray`); the keyword when it gives no name.
    pub label: String,
    /// The driver's group (`Paper Handling`); empty outside a group.
    pub group: String,
    /// The choices: keyword and label (`Tray2`, `Tray 2`).
    pub choices: Vec<(String, String)>,
    /// The choice the printer uses when a job doesn't say.
    pub default: String,
}

/// Options PdfCraft's Print dialog sets itself (paper, two-sided, collation), so the driver's
/// copies of them are neither shown nor sent.
pub const HANDLED_OPTIONS: &[&str] = &["PageSize", "PageRegion", "Duplex", "Collate", "ImageableArea", "PaperDimension"];

/// Caps on what a PPD may make the dialog hold (PPDs are text files from the printer vendor).
const MAX_OPTIONS: usize = 400;
const MAX_CHOICES: usize = 400;

/// A PPD keyword: printable ASCII without spaces, `/`, `:` or `=`.
fn is_ppd_keyword(s: &str) -> bool {
    !s.is_empty() && s.len() <= 64 && s.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b':' | b'=' | b'"'))
}

/// `Name/Label` → (`Name`, `Label`), the label falling back to the name.
fn name_label(s: &str) -> (&str, &str) {
    match s.split_once('/') {
        Some((n, l)) if !l.trim().is_empty() => (n.trim(), l.trim()),
        Some((n, _)) => (n.trim(), n.trim()),
        None => (s.trim(), s.trim()),
    }
}

/// The job options a PPD offers, in file order, without the installable-hardware group and the
/// options PdfCraft sets itself ([`HANDLED_OPTIONS`]). Lenient: malformed lines are skipped.
pub fn parse_ppd(ppd: &str) -> Vec<PrinterOption> {
    let mut out: Vec<PrinterOption> = Vec::new();
    let (mut group, mut group_key) = (String::new(), String::new());
    let mut open: Option<PrinterOption> = None;
    for line in ppd.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("*OpenGroup:") {
            let (k, l) = name_label(rest);
            (group_key, group) = (k.to_string(), l.to_string());
        } else if line.starts_with("*CloseGroup") {
            (group_key, group) = (String::new(), String::new());
        } else if let Some(rest) = line.strip_prefix("*OpenUI") {
            let Some((head, _kind)) = rest.trim().rsplit_once(':') else { continue };
            let (key, label) = name_label(head.trim().trim_start_matches('*'));
            open = is_ppd_keyword(key).then(|| PrinterOption {
                key: key.to_string(),
                label: label.to_string(),
                group: group.clone(),
                choices: Vec::new(),
                default: String::new(),
            });
        } else if line.starts_with("*CloseUI") {
            if let Some(o) = open.take()
                && !o.choices.is_empty()
                && group_key != "InstallableOptions"
                && !HANDLED_OPTIONS.contains(&o.key.as_str())
                && out.len() < MAX_OPTIONS
            {
                out.push(o);
            }
        } else if let Some(o) = open.as_mut() {
            let Some(rest) = line.strip_prefix('*') else { continue };
            if let Some(value) = rest.strip_prefix("Default").and_then(|r| r.strip_prefix(o.key.as_str())).and_then(|r| r.strip_prefix(':')) {
                o.default = value.trim().to_string();
            } else if let Some(rest) = rest.strip_prefix(o.key.as_str()).and_then(|r| r.strip_prefix(' ')) {
                // `*InputSlot Tray2/Tray 2: "<< ... >>"`; the code after the colon is the driver's.
                let head = rest.split_once(':').map_or(rest, |(h, _)| h);
                let (choice, label) = name_label(head);
                if is_ppd_keyword(choice) && o.choices.len() < MAX_CHOICES && o.choices.iter().all(|(c, _)| c != choice) {
                    o.choices.push((choice.to_string(), label.to_string()));
                }
            }
        }
    }
    for o in &mut out {
        if !o.choices.iter().any(|(c, _)| *c == o.default) {
            o.default = o.choices.first().map(|(c, _)| c.clone()).unwrap_or_default();
        }
    }
    out
}

/// The current choice of each option in `lpoptions -p NAME -l` output (`Key/Label: a *b c`): the
/// queue's defaults, including the user's own (`~/.cups/lpoptions`).
pub fn parse_lpoptions(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter_map(|l| {
            let (head, choices) = l.split_once(':')?;
            let (key, _) = name_label(head);
            let current = choices.split_whitespace().find_map(|c| c.strip_prefix('*'))?;
            Some((key.to_string(), current.to_string()))
        })
        .collect()
}

/// A PPD's text: UTF-8, or ISO Latin-1 (the PPD default) when it isn't valid UTF-8.
pub fn ppd_text(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    }
}

/// The driver's job options for a printer (CUPS: its PPD, with the queue's current defaults);
/// empty when it has none or on other platforms. Reads files and runs `lpoptions`, so call it
/// when the user asks, not every frame.
pub fn printer_options(printer: &str) -> Vec<PrinterOption> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        // Queue names never contain `/`; anything else is not a queue and not a path to follow.
        if !is_ppd_keyword(printer) {
            return Vec::new();
        }
        const PPD_CAP: u64 = 16 << 20;
        let ppd = std::fs::File::open(format!("/etc/cups/ppd/{printer}.ppd")).ok().and_then(|f| {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            f.take(PPD_CAP).read_to_end(&mut bytes).ok().map(|_| bytes)
        });
        let mut options = ppd.map(|b| parse_ppd(&ppd_text(&b))).unwrap_or_default();
        let current = std::process::Command::new("lpoptions")
            .args(["-p", printer, "-l"])
            .output()
            .map(|o| parse_lpoptions(&String::from_utf8_lossy(&o.stdout)))
            .unwrap_or_default();
        for (key, value) in current {
            if let Some(o) = options.iter_mut().find(|o| o.key == key)
                && o.choices.iter().any(|(c, _)| *c == value)
            {
                o.default = value;
            }
        }
        options
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        let _ = printer;
        Vec::new()
    }
}

/// Whether [`open_printer_preferences`] can show the driver's own settings window (Windows).
pub const HAS_PRINTER_PREFERENCES: bool = cfg!(windows);

/// Open the printer driver's Printing Preferences window (Windows: `printui.dll`). What the user
/// sets there is saved as that printer's defaults for this Windows user, which every job starts
/// from; the Print dialog's copies, two-sided, grayscale and paper still apply on top.
pub fn open_printer_preferences(printer: &str) -> Result<(), PrintError> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // printui takes the name in quotes; Windows printer names can't contain one anyway.
        if printer.is_empty() || printer.contains('"') {
            return Err(PrintError::Spool("not a printer name".into()));
        }
        let root = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        let rundll = std::path::Path::new(&root).join("System32").join("rundll32.exe");
        std::process::Command::new(rundll)
            .raw_arg("printui.dll,PrintUIEntry")
            .raw_arg("/e")
            .raw_arg("/n")
            .raw_arg(format!("\"{printer}\""))
            .spawn()
            .map(|_| ())
            .map_err(|e| PrintError::Spool(format!("the printer's preferences could not be opened: {e}")))
    }
    #[cfg(not(windows))]
    {
        let _ = printer;
        Err(PrintError::Spool("printer preferences open only on Windows; use the printer's options instead".into()))
    }
}

/// `lpstat -p -d`, forced to print untranslated messages so [`parse_lpstat`] can read them.
///
/// `LC_ALL`/`LANG=C` is enough on Linux. macOS CUPS ignores them and follows the user's
/// interface language (`AppleLanguages`) unless `SOFTWARE` is set, in which case it uses `LANG`.
pub fn lpstat_command() -> std::process::Command {
    let mut c = std::process::Command::new("lpstat");
    c.args(["-p", "-d"]).env("LC_ALL", "C").env("LANG", "C").env("SOFTWARE", "PdfCraft");
    c
}

/// The printers the system knows (empty when there are none or no spooler).
pub fn printers() -> Vec<Printer> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        match lpstat_command().output() {
            Ok(o) => parse_lpstat(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => Vec::new(),
        }
    }
    #[cfg(windows)]
    {
        match win_printers_command().output() {
            Ok(o) => parse_win_printers(&String::from_utf8_lossy(&o.stdout)),
            Err(_) => Vec::new(),
        }
    }
    #[cfg(not(any(windows, all(unix, not(target_arch = "wasm32")))))]
    {
        Vec::new()
    }
}

/// Send a print-ready PDF to the spooler. Returns the spooler's message (the job id).
pub fn submit(pdf: &[u8], job: &Job) -> Result<String, PrintError> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        submit_via(std::process::Command::new("lp"), pdf, job)
    }
    #[cfg(windows)]
    {
        submit_windows(pdf, job)
    }
    #[cfg(not(any(windows, all(unix, not(target_arch = "wasm32")))))]
    {
        let _ = (pdf, job);
        Err(PrintError::Spool("printing to a printer isn't available on this platform yet; save the print-ready PDF instead".into()))
    }
}

/// [`submit`] with the spooler command given, so tests can stand in for `lp`.
///
/// The job goes to `lp` on stdin, never through a file: a predictable job folder in a shared
/// temp directory lets another local user read printed documents or swap the file before `lp`
/// reads it.
#[cfg(any(test, all(unix, not(target_arch = "wasm32"))))]
pub(crate) fn submit_via(mut lp: std::process::Command, pdf: &[u8], job: &Job) -> Result<String, PrintError> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = lp
        .args(lp_args(job))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| PrintError::Spool(format!("the print spooler is not available: {e}")))?;
    let stdin = child.stdin.take();
    // Feed the job on its own thread while collecting the output, so neither side can fill a
    // pipe and wait on the other. Dropping `stdin` at the end closes it: the end of the job.
    let fed_and_out = std::thread::scope(|s| {
        let feeder = std::thread::Builder::new().name("print job".into()).spawn_scoped(s, move || match stdin {
            Some(mut stdin) => stdin.write_all(pdf),
            None => Err(std::io::Error::other("no pipe to the spooler")),
        });
        match feeder {
            Ok(feeder) => {
                let out = child.wait_with_output();
                Ok((feeder.join(), out))
            }
            Err(e) => {
                // No thread to feed it: stop `lp` rather than leave it with an empty job.
                let _ = child.kill();
                let _ = child.wait();
                Err(PrintError::Spool(format!("the print job could not be sent to the spooler: {e}")))
            }
        }
    });
    let (fed, out) = fed_and_out?;
    let out = out.map_err(|e| PrintError::Spool(format!("the print spooler is not available: {e}")))?;
    if !out.status.success() {
        // `lp` may refuse before reading the job (an unknown printer); its message says why,
        // and the broken pipe that leaves behind does not.
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(PrintError::Spool(if err.is_empty() { "the print job was refused".into() } else { err }));
    }
    match fed {
        Ok(Ok(())) => Ok(job_message(&out.stdout)),
        Ok(Err(e)) => Err(PrintError::Spool(format!("the print job could not be sent to the spooler: {e}"))),
        Err(_) => Err(PrintError::Spool("the print job could not be sent to the spooler".into())),
    }
}

/// `lp`'s reply ("request id is Office-12"), without the "(0 file(s))" CUPS adds when the job came
/// on stdin: it reads as if nothing was sent.
#[cfg(any(test, all(unix, not(target_arch = "wasm32"))))]
pub(crate) fn job_message(stdout: &[u8]) -> String {
    let s = String::from_utf8_lossy(stdout);
    let s = s.trim();
    s.strip_suffix("(0 file(s))").map_or(s, str::trim_end).to_string()
}

// ---- Windows -------------------------------------------------------------------------------

/// Resolution the sheets are rasterized at for a Windows print job.
#[cfg(any(test, windows))]
const WIN_DPI: f32 = 300.0;

/// Printers and which one is the default, one per line as `True<TAB>Name` (`False` for the rest).
/// `InstalledPrinters` asks the spooler directly; WMI's `Win32_Printer` returns the same list but
/// takes about five times as long, and the Print dialog waits for it.
#[cfg(windows)]
const WIN_PRINTERS_SCRIPT: &str = "[Console]::OutputEncoding=[Text.Encoding]::UTF8; \
Add-Type -AssemblyName System.Drawing; \
$d = (New-Object System.Drawing.Printing.PrinterSettings).PrinterName; \
foreach ($n in [System.Drawing.Printing.PrinterSettings]::InstalledPrinters) { \"$($n -eq $d)`t$n\" }";

/// Parse `WIN_PRINTERS_SCRIPT`'s output.
pub fn parse_win_printers(out: &str) -> Vec<Printer> {
    out.lines()
        .filter_map(|l| {
            let (default, name) = l.trim_end_matches('\r').split_once('\t')?;
            let name = name.trim();
            (!name.is_empty()).then(|| Printer { name: name.to_string(), default: default.trim().eq_ignore_ascii_case("true") })
        })
        .collect()
}

/// The PowerShell script that prints a job. It reads the job from stdin (see [`write_frames`])
/// and the options from `PDFCRAFT_*` environment variables, so no document or printer name is
/// ever spliced into the script text or written to a file.
#[cfg(windows)]
const WIN_PRINT_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
try {
  Add-Type -AssemblyName System.Drawing
  $in = [Console]::OpenStandardInput()
  function ReadExact([int]$n) {
    $b = New-Object byte[] $n; $o = 0
    while ($o -lt $n) { $r = $in.Read($b, $o, $n - $o); if ($r -le 0) { throw 'the print data ended early' }; $o += $r }
    ,$b
  }
  $script:pages = [BitConverter]::ToInt32((ReadExact 4), 0)
  $script:idx = 0; $script:bmp = $null; $script:err = $null
  $pd = New-Object System.Drawing.Printing.PrintDocument
  $pd.DocumentName = $env:PDFCRAFT_TITLE
  if ($env:PDFCRAFT_PRINTER) { $pd.PrinterSettings.PrinterName = $env:PDFCRAFT_PRINTER }
  if (-not $pd.PrinterSettings.IsValid) { throw 'the printer is not available' }
  $pd.PrintController = New-Object System.Drawing.Printing.StandardPrintController
  $pd.PrinterSettings.Copies = [int16]$env:PDFCRAFT_COPIES
  $pd.PrinterSettings.Collate = ($env:PDFCRAFT_COLLATE -eq '1')
  if ($env:PDFCRAFT_DUPLEX -ne 'off' -and $pd.PrinterSettings.CanDuplex) {
    $pd.PrinterSettings.Duplex = if ($env:PDFCRAFT_DUPLEX -eq 'long') { [System.Drawing.Printing.Duplex]::Vertical } else { [System.Drawing.Printing.Duplex]::Horizontal }
  }
  $gray = ($env:PDFCRAFT_GRAY -eq '1')
  if ($gray) { $pd.DefaultPageSettings.Color = $false }
  $pd.add_QueryPageSettings({
    param($s, $e)
    try {
      $h = ReadExact 16
      $wpx = [BitConverter]::ToInt32($h, 0); $hpx = [BitConverter]::ToInt32($h, 4)
      $script:wh = [BitConverter]::ToInt32($h, 8); $script:hh = [BitConverter]::ToInt32($h, 12)
      $data = ReadExact ($wpx * $hpx * 4)
      if ($script:bmp) { $script:bmp.Dispose() }
      $bm = New-Object System.Drawing.Bitmap($wpx, $hpx, [System.Drawing.Imaging.PixelFormat]::Format32bppArgb)
      $bd = $bm.LockBits((New-Object System.Drawing.Rectangle(0, 0, $wpx, $hpx)), [System.Drawing.Imaging.ImageLockMode]::WriteOnly, $bm.PixelFormat)
      [Runtime.InteropServices.Marshal]::Copy($data, 0, $bd.Scan0, $data.Length)
      $bm.UnlockBits($bd)
      $script:bmp = $bm
      $land = $script:wh -gt $script:hh
      $pw = if ($land) { $script:hh } else { $script:wh }
      $ph = if ($land) { $script:wh } else { $script:hh }
      $paper = $null
      foreach ($p in $pd.PrinterSettings.PaperSizes) {
        if ([math]::Abs($p.Width - $pw) -le 6 -and [math]::Abs($p.Height - $ph) -le 6) { $paper = $p; break }
      }
      if (-not $paper) { $paper = New-Object System.Drawing.Printing.PaperSize('PdfCraft', $pw, $ph) }
      $e.PageSettings.PaperSize = $paper
      $e.PageSettings.Landscape = $land
      if ($gray) { $e.PageSettings.Color = $false }
    } catch { $script:err = $_.Exception.Message; $e.Cancel = $true }
  })
  $pd.add_PrintPage({
    param($s, $e)
    try {
      $g = $e.Graphics
      $g.TranslateTransform(-$e.PageSettings.HardMarginX, -$e.PageSettings.HardMarginY)
      $g.InterpolationMode = [System.Drawing.Drawing2D.InterpolationMode]::HighQualityBicubic
      $g.DrawImage($script:bmp, 0, 0, $script:wh, $script:hh)
      $script:idx++
      $e.HasMorePages = ($script:idx -lt $script:pages)
    } catch { $script:err = $_.Exception.Message; $e.Cancel = $true; $e.HasMorePages = $false }
  })
  $pd.Print()
  if ($script:err) { throw $script:err }
  [Console]::Out.WriteLine("$($script:idx) page(s) sent")
} catch {
  [Console]::Error.WriteLine($_.Exception.Message)
  exit 1
}
"#;

/// Why [`write_frames`] stopped.
#[cfg(any(test, windows))]
#[derive(Debug)]
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) enum Frames {
    /// The document can't be printed (nothing to print, a page that won't render).
    Document(String),
    /// The reader went away, usually because the print script failed first and said why.
    Pipe(String),
}

/// Rasterize `pdf` and write it as the stream `WIN_PRINT_SCRIPT` reads: the page count (`i32`),
/// then per page `[width px, height px, width and height in 1/100 inch]` (four `i32`) followed by
/// `width * height` BGRA pixels. All little endian. Returns the page count.
#[cfg(any(test, windows))]
pub(crate) fn write_frames(pdf: &[u8], out: &mut impl std::io::Write) -> Result<usize, Frames> {
    use pdfcraft_render::{PageRenderer, RenderConfig, RenderRequest};
    let mut renderer = PageRenderer::new(std::sync::Arc::new(pdf.to_vec()), RenderConfig::default());
    let pages = renderer.page_count();
    if pages == 0 {
        return Err(Frames::Document("there are no pages to print".into()));
    }
    let io = |e: std::io::Error| Frames::Pipe(format!("the print job could not be sent to the spooler: {e}"));
    out.write_all(&i32::try_from(pages).unwrap_or(i32::MAX).to_le_bytes()).map_err(io)?;
    let scale = WIN_DPI / 72.0;
    for page in 0..pages {
        let r = renderer.render(RenderRequest { page, scale, ..Default::default() });
        if let Some(e) = r.error {
            return Err(Frames::Document(format!("page {} could not be rendered: {e}", page + 1)));
        }
        // A page past the renderer's size caps comes back smaller than asked for and prints
        // proportionally smaller; such pages are far beyond any paper.
        let hundredths = |px: u32| i32::try_from((f64::from(px) * 100.0 / f64::from(WIN_DPI)).round() as u64).unwrap_or(i32::MAX);
        let (w, h) = (
            i32::try_from(r.width).map_err(|_| Frames::Document("page too large".into()))?,
            i32::try_from(r.height).map_err(|_| Frames::Document("page too large".into()))?,
        );
        for v in [w, h, hundredths(r.width), hundredths(r.height)] {
            out.write_all(&v.to_le_bytes()).map_err(io)?;
        }
        // The page is opaque (white background), so premultiplied RGBA is plain RGBA: swap to BGRA.
        let bgra: Vec<u8> = r.rgba.as_chunks::<4>().0.iter().flat_map(|&[r, g, b, _]| [b, g, r, 255]).collect();
        out.write_all(&bgra).map_err(io)?;
    }
    out.flush().map_err(io)?;
    Ok(pages)
}

/// The `PDFCRAFT_*` environment `WIN_PRINT_SCRIPT` reads.
#[cfg(any(test, windows))]
pub(crate) fn win_job_env(job: &Job) -> Vec<(&'static str, String)> {
    vec![
        ("PDFCRAFT_PRINTER", job.printer.clone().unwrap_or_default()),
        ("PDFCRAFT_COPIES", job.copies.clamp(1, 999).to_string()),
        ("PDFCRAFT_COLLATE", if job.collate { "1" } else { "0" }.into()),
        (
            "PDFCRAFT_DUPLEX",
            match job.duplex {
                Duplex::Off => "off",
                Duplex::LongEdge => "long",
                Duplex::ShortEdge => "short",
            }
            .into(),
        ),
        ("PDFCRAFT_GRAY", if job.grayscale { "1" } else { "0" }.into()),
        ("PDFCRAFT_TITLE", job.title.clone()),
    ]
}

/// `-EncodedCommand` takes the script as base64 of its UTF-16LE text.
#[cfg(any(test, windows))]
pub(crate) fn encode_command(script: &str) -> String {
    const ABC: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for c in bytes.chunks(3) {
        let n = (u32::from(c[0]) << 16) | (u32::from(c.get(1).copied().unwrap_or(0)) << 8) | u32::from(c.get(2).copied().unwrap_or(0));
        for i in 0..4 {
            if i <= c.len() {
                out.push(ABC[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(windows)]
fn powershell(script: &str) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    let mut c = std::process::Command::new("powershell.exe");
    c.args(["-NoProfile", "-NonInteractive", "-EncodedCommand", &encode_command(script)]);
    // CREATE_NO_WINDOW: no console flashes up from the GUI app.
    c.creation_flags(0x0800_0000);
    c
}

#[cfg(windows)]
fn win_printers_command() -> std::process::Command {
    powershell(WIN_PRINTERS_SCRIPT)
}

#[cfg(windows)]
fn submit_windows(pdf: &[u8], job: &Job) -> Result<String, PrintError> {
    use std::process::Stdio;
    let mut cmd = powershell(WIN_PRINT_SCRIPT);
    cmd.envs(win_job_env(job)).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| PrintError::Spool(format!("the print spooler is not available: {e}")))?;
    let stdin = child.stdin.take();
    // Render and feed on a thread while collecting the output, so neither side can fill a pipe
    // and wait on the other. Dropping `stdin` at the end of the thread closes it.
    let (fed, out) = std::thread::scope(|s| {
        let feeder = std::thread::Builder::new().name("print job".into()).spawn_scoped(s, move || match stdin {
            Some(mut stdin) => write_frames(pdf, &mut std::io::BufWriter::new(&mut stdin)),
            None => Err(Frames::Pipe("no pipe to the spooler".into())),
        });
        match feeder {
            Ok(feeder) => {
                let out = child.wait_with_output();
                (feeder.join().unwrap_or_else(|_| Err(Frames::Document("the print job could not be prepared".into()))), out)
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                (Err(Frames::Pipe(format!("the print job could not be sent to the spooler: {e}"))), Err(e))
            }
        }
    });
    // A page that can't be rendered is the root cause; PowerShell's "data ended early" is not.
    if let Err(Frames::Document(e)) = &fed {
        return Err(PrintError::Spool(e.clone()));
    }
    let out = out.map_err(|e| PrintError::Spool(format!("the print spooler is not available: {e}")))?;
    if !out.status.success() {
        // The script may refuse before reading the job (an unknown printer); its message says
        // why, and the broken pipe that leaves behind does not.
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(PrintError::Spool(if err.is_empty() { "the print job was refused".into() } else { err }));
    }
    if let Err(Frames::Pipe(e)) = fed {
        return Err(PrintError::Spool(e));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}
