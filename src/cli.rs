//! Command-line options, used when another program starts ScanDesk to get a
//! scanned document back.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

pub const USAGE: &str = "\
Usage: scandesk [OPTIONS] [FILE | FOLDER]

  FILE | FOLDER         Save folder and document name for the scan. An existing
                        file continues its numbering (invoice3.pdf -> invoice4).

Options for calling ScanDesk from another program:
  --quit-after-save     Close ScanDesk once the document is saved.
  --result-file PATH    Write the full paths of the saved files to PATH, one per
                        line (UTF-8). The file is replaced atomically after
                        every save.
  -h, --help            Show this help.

The paths of saved files are always printed to standard output, one per line.
With --quit-after-save or --result-file, the exit code is 0 if a document was
saved and 1 if ScanDesk was closed without saving.";

#[derive(Clone, Debug, Default, PartialEq)]
pub struct StartOptions {
    /// File or folder that sets the save folder and document name.
    pub file: Option<PathBuf>,
    /// Where to write the paths of the saved files.
    pub result_file: Option<PathBuf>,
    /// Close the app after a successful save.
    pub quit_after_save: bool,
    pub help: bool,
}

impl StartOptions {
    /// Parses the arguments after the program name.
    pub fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, String> {
        let mut opts = Self::default();
        let mut args = args.into_iter();
        while let Some(arg) = args.next() {
            let text = arg.to_string_lossy();
            match text.as_ref() {
                "-h" | "--help" => opts.help = true,
                "--quit-after-save" => opts.quit_after_save = true,
                "--result-file" => {
                    let path = args.next().ok_or("--result-file needs a path")?;
                    opts.result_file = Some(PathBuf::from(path));
                }
                s if s.starts_with("--result-file=") => {
                    opts.result_file = Some(PathBuf::from(&s["--result-file=".len()..]));
                }
                // Added by macOS when an app is launched from Finder.
                s if s.starts_with("-psn_") => {}
                s if s.starts_with('-') => return Err(format!("unknown option: {s}")),
                _ if opts.file.is_none() => opts.file = Some(PathBuf::from(arg)),
                _ => return Err(format!("unexpected argument: {text}")),
            }
        }
        Ok(opts)
    }

    /// True if a calling program waits for the result.
    pub fn called_by_program(&self) -> bool {
        self.quit_after_save || self.result_file.is_some()
    }
}

/// Reports saved files to the calling program: prints them to standard output
/// and, if requested, writes them to the result file.
pub fn report_saved(opts: &StartOptions, all_saved: &[PathBuf], new: &[PathBuf]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    for p in new {
        // A closed pipe must not stop the app.
        let _ = writeln!(out, "{}", p.display());
    }
    let _ = out.flush();
    if let Some(file) = &opts.result_file {
        write_atomic(file, &paths_text(all_saved))?;
    }
    Ok(())
}

fn paths_text(paths: &[PathBuf]) -> String {
    paths.iter().map(|p| format!("{}\n", p.display())).collect()
}

/// Writes via a temporary file in the same folder and renames it, so a caller
/// never reads a half-written file.
fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<StartOptions, String> {
        StartOptions::parse(args.iter().map(OsString::from))
    }

    #[test]
    fn parses_options() {
        let o = parse(&["--quit-after-save", "--result-file", "/tmp/r.txt", "a b.pdf"]).unwrap();
        assert!(o.quit_after_save && o.called_by_program());
        assert_eq!(o.result_file, Some(PathBuf::from("/tmp/r.txt")));
        assert_eq!(o.file, Some(PathBuf::from("a b.pdf")));
        assert_eq!(parse(&["--result-file=x"]).unwrap().result_file, Some(PathBuf::from("x")));
        assert_eq!(parse(&["-psn_0_12345"]).unwrap(), StartOptions::default());
        assert!(!parse(&["doc.pdf"]).unwrap().called_by_program());
        assert!(parse(&["--result-file"]).is_err());
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["a", "b"]).is_err());
        assert!(parse(&["-h"]).unwrap().help);
    }

    #[test]
    fn result_file_lists_all_saved_paths() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("result.txt");
        let opts = StartOptions { result_file: Some(file.clone()), ..Default::default() };
        let a = PathBuf::from("/x/Rechnung Mai.pdf");
        let b = PathBuf::from("/x/scan2.pdf");
        report_saved(&opts, std::slice::from_ref(&a), std::slice::from_ref(&a)).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "/x/Rechnung Mai.pdf\n");
        report_saved(&opts, &[a, b.clone()], &[b]).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "/x/Rechnung Mai.pdf\n/x/scan2.pdf\n");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temporary file left");
    }
}
