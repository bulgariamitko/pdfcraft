//! System print spooler supplied by craft-print. PDF composition and permission checks
//! stay in PdfCraft; these adapters retain the application's print-error surface.

pub use craft_print::{
    Duplex, HANDLED_OPTIONS, HAS_PRINTER_PREFERENCES, Job, Printer, PrinterOption, Scaling, lp_args, lpstat_command, parse_lpoptions, parse_lpstat,
    parse_lpstat_e, parse_ppd, parse_windows_printers, ppd_text, printers, printers_parsed,
};

/// The driver's job options for a printer (CUPS: its PPD, with the queue's current defaults);
/// empty when it has none or on other platforms. Reads files and runs `lpoptions`, so call it
/// when the user asks, not every frame.
pub fn printer_options(printer: &str) -> Vec<PrinterOption> {
    printer_options_in(printer, "en")
}

/// [`printer_options`] with the driver's own names in `lang` (a UI language code such as `bg`,
/// `pt-br` or `zh-hans`) where its PPD has them (`*bg.Translation`, `*bg.InputSlot Tray2/…`);
/// the driver's default names otherwise.
pub fn printer_options_in(printer: &str, lang: &str) -> Vec<PrinterOption> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        printer_options_via(printer, std::path::Path::new("/etc/cups/ppd"), std::process::Command::new("lpoptions"), lang)
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        let _ = lang;
        craft_print::printer_options(printer)
    }
}

/// Read a destination's options with the PPD directory and command supplied, so tests need
/// neither a system queue nor a physical printer. CUPS instances share their queue's PPD but
/// have their own defaults, queried with the complete `queue/instance` destination.
#[cfg(all(unix, not(target_arch = "wasm32")))]
pub(crate) fn printer_options_via(printer: &str, ppd_dir: &std::path::Path, mut lpoptions: std::process::Command, lang: &str) -> Vec<PrinterOption> {
    let (queue, instance) = printer.split_once('/').map_or((printer, None), |(q, i)| (q, Some(i)));
    // Retain the conservative name checks, applied to each component. Neither component is
    // a path: reject dot components, backslashes and any additional separator before I/O.
    let valid = |name: &str| {
        !name.is_empty()
            && name.len() <= 64
            && !matches!(name, "." | "..")
            && name.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'\\' | b':' | b'=' | b'"'))
    };
    if !valid(queue) || instance.is_some_and(|name| !valid(name)) {
        return Vec::new();
    }
    const PPD_CAP: u64 = 16 << 20;
    let ppd = std::fs::File::open(ppd_dir.join(format!("{queue}.ppd"))).ok().and_then(|f| {
        use std::io::Read as _;
        let mut bytes = Vec::new();
        f.take(PPD_CAP).read_to_end(&mut bytes).ok().map(|_| bytes)
    });
    let mut options = ppd.as_deref().map(|b| parse_ppd(&ppd_text(b))).unwrap_or_default();
    if let Some(b) = &ppd {
        localize(&mut options, b, lang);
    }
    let current = lpoptions.args(["-p", printer, "-l"]).output().map(|o| parse_lpoptions(&String::from_utf8_lossy(&o.stdout))).unwrap_or_default();
    for (key, value) in current {
        if let Some(o) = options.iter_mut().find(|o| o.key == key)
            && o.choices.iter().any(|(c, _)| *c == value)
        {
            o.default = value;
        }
    }
    options
}

pub fn submit(pdf: &[u8], job: &Job) -> Result<String, crate::PrintError> {
    craft_print::submit(pdf, job).map_err(|e| crate::PrintError::Spool(e.to_string()))
}

pub fn open_printer_preferences(printer: &str) -> Result<(), crate::PrintError> {
    craft_print::open_printer_preferences(printer).map_err(|e| crate::PrintError::Spool(e.to_string()))
}

/// The paper a printer uses when a job doesn't say, as a [`crate::PAPERS`] index: on CUPS the
/// destination's `media` default, else its PPD's page size. `None` when it isn't one of those
/// papers, can't be read, or on other platforms. Runs `lpoptions`, so call it once per printer,
/// not every frame.
pub fn default_paper(printer: &str) -> Option<usize> {
    #[cfg(all(unix, not(target_arch = "wasm32")))]
    {
        let queue = printer.split_once('/').map_or(printer, |(q, _)| q);
        if queue.is_empty()
            || queue.len() > 64
            || matches!(queue, "." | "..")
            || !queue.bytes().all(|b| b.is_ascii_graphic() && !matches!(b, b'/' | b'\\' | b':'))
        {
            return None;
        }
        let run = |args: &[&str]| {
            std::process::Command::new("lpoptions")
                .args(args)
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default()
        };
        let defaults = run(&["-p", printer]);
        let listed = run(&["-p", printer, "-l"]);
        const PPD_CAP: u64 = 16 << 20;
        let ppd = std::fs::File::open(std::path::Path::new("/etc/cups/ppd").join(format!("{queue}.ppd"))).ok().and_then(|f| {
            use std::io::Read as _;
            let mut bytes = Vec::new();
            f.take(PPD_CAP).read_to_end(&mut bytes).ok().map(|_| ppd_text(&bytes))
        });
        default_paper_parsed(&defaults, &listed, ppd.as_deref())
    }
    #[cfg(not(all(unix, not(target_arch = "wasm32"))))]
    {
        let _ = printer;
        None
    }
}

/// [`default_paper`] from `lpoptions -p` (`defaults`), `lpoptions -p -l` (`listed`) and the PPD.
pub fn default_paper_parsed(defaults: &str, listed: &str, ppd: Option<&str>) -> Option<usize> {
    let media = defaults
        .split_whitespace()
        .find_map(|w| w.strip_prefix("media="))
        .map(|m| m.trim_matches(|c| c == '\'' || c == '"'))
        .and_then(|m| m.split(',').next())
        .filter(|m| !m.is_empty());
    if let Some(paper) =
        media.and_then(|m| pwg_size(m).or_else(|| ppd.and_then(|p| ppd_dimension(p, m))).or_else(|| named_size(m))).and_then(crate::matching_paper)
    {
        return Some(paper);
    }
    let page_size = parse_lpoptions(listed).into_iter().find(|(k, _)| k == "PageSize").map(|(_, v)| v)?;
    ppd.and_then(|p| ppd_dimension(p, &page_size)).or_else(|| named_size(&page_size)).and_then(crate::matching_paper)
}

/// A PWG self-describing media name's size in points (`iso_a5_148x210mm`, `na_letter_8.5x11in`).
fn pwg_size(name: &str) -> Option<(f64, f64)> {
    let dims = name.rsplit('_').next()?;
    let (dims, per_unit) = if let Some(d) = dims.strip_suffix("mm") { (d, 72.0 / 25.4) } else { (dims.strip_suffix("in")?, 72.0) };
    let (w, h) = dims.split_once('x')?;
    let (w, h) = (w.parse::<f64>().ok()? * per_unit, h.parse::<f64>().ok()? * per_unit);
    (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w, h))
}

/// `*PaperDimension name/label: "w h"` in the PPD, in points.
fn ppd_dimension(ppd: &str, name: &str) -> Option<(f64, f64)> {
    ppd.lines().find_map(|line| {
        let rest = line.strip_prefix("*PaperDimension ")?.strip_prefix(name)?;
        let value = rest.strip_prefix(':').or_else(|| rest.strip_prefix('/').and_then(|r| r.split_once(':').map(|(_, v)| v)))?;
        let mut n = value.trim().trim_matches('"').split_whitespace().map(|v| v.parse::<f64>().ok());
        let (w, h) = (n.next()??, n.next()??);
        (w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0).then_some((w, h))
    })
}

/// The sizes [`crate::cups_media`] names, read back.
fn named_size(name: &str) -> Option<(f64, f64)> {
    crate::PAPERS.iter().enumerate().find(|(i, _)| crate::cups_media(*i).is_some_and(|m| m.eq_ignore_ascii_case(name))).map(|(_, p)| p.1)
}

/// The PPD language codes (`*cupsLanguages`, the `ll` of `*ll.Translation`) to try for a UI
/// language code, most specific first: `pt-br` → `pt_BR`, `pt`; `zh-hans` → `zh_CN`, `zh`.
fn ppd_languages(lang: &str) -> Vec<String> {
    let lang = lang.to_ascii_lowercase();
    let mut parts = lang.split(['-', '_']).filter(|p| !p.is_empty());
    let Some(primary) = parts.next() else { return Vec::new() };
    let region = match (primary, parts.next()) {
        ("zh", Some("hans")) => Some("CN".to_string()),
        ("zh", Some("hant")) => Some("TW".to_string()),
        (_, Some(r)) if r.len() == 2 => Some(r.to_ascii_uppercase()),
        _ => None,
    };
    region.map(|r| format!("{primary}_{r}")).into_iter().chain(std::iter::once(primary.to_string())).collect()
}

/// Give `options` the driver's own names in `lang` from the PPD's localization lines
/// (`*ll.Translation Key/Label: ""` for options and groups, `*ll.Key choice/Label: ""` for
/// choices). Each line is read as UTF-8 on its own: drivers write them so in files whose other
/// text is Latin-1. Names with no translation, or an empty one, stay as they are.
pub(crate) fn localize(options: &mut [PrinterOption], ppd: &[u8], lang: &str) {
    use std::collections::HashMap;
    let Some(code) = ppd_languages(lang).into_iter().find(|code| {
        let prefix = format!("*{code}.");
        ppd.split(|b| *b == b'\n').any(|l| l.starts_with(prefix.as_bytes()))
    }) else {
        return;
    };
    if code == "en" {
        return;
    }
    let prefix = format!("*{code}.");
    let mut names: HashMap<String, String> = HashMap::new();
    let mut choices: HashMap<(String, String), String> = HashMap::new();
    let mut groups: HashMap<String, String> = HashMap::new();
    for line in ppd.split(|b| *b == b'\n') {
        let Some(text) = std::str::from_utf8(line).ok() else { continue };
        let text = text.trim_end_matches('\r');
        if let Some(rest) = text.strip_prefix("*OpenGroup:") {
            // Group label → keyword, since options carry their group's label.
            let rest = rest.trim();
            if let Some((key, label)) = rest.split_once('/') {
                groups.insert(label.trim().to_string(), key.trim().to_string());
            }
            continue;
        }
        let Some(rest) = text.strip_prefix(prefix.as_str()) else { continue };
        let Some((head, _)) = rest.split_once(':') else { continue };
        let Some((main, spec)) = head.split_once(' ') else { continue };
        let Some((key, label)) = spec.split_once('/') else { continue };
        let label = label.trim();
        if label.is_empty() {
            continue;
        }
        if main == "Translation" {
            names.insert(key.trim().to_string(), label.to_string());
        } else {
            choices.insert((main.to_string(), key.trim().to_string()), label.to_string());
        }
    }
    for o in options.iter_mut() {
        if let Some(label) = names.get(&o.key) {
            o.label = label.clone();
        }
        if let Some(label) = groups.get(&o.group).and_then(|key| names.get(key)) {
            o.group = label.clone();
        }
        for (choice, label) in o.choices.iter_mut() {
            if let Some(t) = choices.get(&(o.key.clone(), choice.clone())) {
                *label = t.clone();
            }
        }
    }
}
