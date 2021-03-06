pub use self::checkstyle::*;
pub use self::diff::*;
pub use self::files::*;
pub use self::json::*;
pub use self::modified_lines::*;
pub use self::stdout::*;

use std::fs;
use std::io::{self, Write};
use std::path::Path;
use std::rc::Rc;

use thiserror::Error;

use crate::formatting::ParseSess;
use crate::{config::FileName, FormatReport, FormatResult, NewlineStyle};

pub mod checkstyle;
pub mod diff;
pub mod files;
pub mod json;
pub mod modified_lines;
pub mod rustfmt_diff;
pub mod stdout;

pub struct FormattedFile<'a> {
    pub filename: &'a FileName,
    pub original_text: &'a str,
    pub formatted_text: &'a str,
}

#[derive(Debug, Default, Clone)]
pub struct EmitterResult {
    pub has_diff: bool,
}

#[derive(Debug, Error)]
pub enum EmitterError {
    #[error("{0}")]
    IoError(#[from] io::Error),
    #[error("{0}")]
    JsonError(#[from] serde_json::Error),
    #[error("invalid input for EmitMode::Files")]
    InvalidInputForFiles,
}

pub trait Emitter {
    fn emit_formatted_file(
        &mut self,
        output: &mut dyn Write,
        formatted_file: FormattedFile<'_>,
    ) -> Result<EmitterResult, EmitterError>;

    fn emit_header(&self, _output: &mut dyn Write) -> Result<(), EmitterError> {
        Ok(())
    }

    fn emit_footer(&self, _output: &mut dyn Write) -> Result<(), EmitterError> {
        Ok(())
    }
}

/// What Rustfmt should emit. Mostly corresponds to the `--emit` command line
/// option.
#[derive(Clone, Copy, Debug)]
pub enum EmitMode {
    /// Emits to files.
    Files,
    /// Writes the output to stdout.
    Stdout,
    /// Unfancy stdout
    Checkstyle,
    /// Writes the resulting diffs in a JSON format. Returns an empty array
    /// `[]` if there were no diffs.
    Json,
    /// Output the changed lines (for internal value only)
    ModifiedLines,
    /// Checks if a diff can be generated. If so, rustfmt outputs a diff and
    /// quits with exit code 1.
    /// This option is designed to be run in CI where a non-zero exit signifies
    /// non-standard code formatting. Used for `--check`.
    Diff,
}

/// Client-preference for coloured output.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// Always use color, whether it is a piped or terminal output
    Always,
    /// Never use color
    Never,
    /// Automatically use color, if supported by terminal
    Auto,
}

impl Color {
    /// Whether we should use a coloured terminal.
    pub fn use_colored_tty(self) -> bool {
        match self {
            Color::Always | Color::Auto => true,
            Color::Never => false,
        }
    }
}

/// How chatty should Rustfmt be?
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Verbosity {
    /// Default.
    Normal,
    /// Emit more.
    Verbose,
    /// Emit as little as possible.
    Quiet,
}

impl Default for Verbosity {
    fn default() -> Self {
        Verbosity::Normal
    }
}

impl std::str::FromStr for EmitMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "files" => Ok(EmitMode::Files),
            "stdout" => Ok(EmitMode::Stdout),
            "checkstyle" => Ok(EmitMode::Checkstyle),
            "json" => Ok(EmitMode::Json),
            _ => Err(format!("unknown emit mode `{}`", s)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct EmitterConfig {
    pub emit_mode: EmitMode,
    pub color: Color,
    pub verbosity: Verbosity,
    pub print_filename: bool,
}

impl Default for EmitterConfig {
    fn default() -> Self {
        EmitterConfig {
            emit_mode: EmitMode::Files,
            color: Color::Auto,
            verbosity: Verbosity::Normal,
            print_filename: false,
        }
    }
}

pub fn emit_format_report<T>(
    format_report: FormatReport,
    out: &mut T,
    config: EmitterConfig,
) -> Result<bool, EmitterError>
where
    T: Write,
{
    let mut emitter = create_emitter(config);
    let mut has_diff = false;

    emitter.emit_header(out)?;
    for (filename, format_result) in format_report.format_result_as_rc().borrow().iter() {
        has_diff |= write_file(None, filename, &format_result, out, &mut *emitter)?.has_diff;
    }
    emitter.emit_footer(out)?;

    Ok(has_diff)
}

pub(crate) fn write_file<T>(
    parse_sess: Option<&ParseSess>,
    filename: &FileName,
    formatted_result: &FormatResult,
    out: &mut T,
    emitter: &mut dyn Emitter,
) -> Result<EmitterResult, EmitterError>
where
    T: Write,
{
    fn ensure_real_path(filename: &FileName) -> &Path {
        match *filename {
            FileName::Real(ref path) => path,
            _ => panic!("cannot format `{}` and emit to files", filename),
        }
    }

    // SourceFile's in the SourceMap will always have Unix-style line endings
    // See: https://github.com/rust-lang/rustfmt/issues/3850
    // So if the user has explicitly overridden the rustfmt `newline_style`
    // config and `filename` is FileName::Real, then we must check the file system
    // to get the original file value in order to detect newline_style conflicts.
    // Otherwise, parse session is around (cfg(not(test))) and newline_style has been
    // left as the default value, then try getting source from the parse session
    // source map instead of hitting the file system. This also supports getting
    // original text for `FileName::Stdin`.
    let original_text =
        if formatted_result.newline_style() != NewlineStyle::Auto && *filename != FileName::Stdin {
            Rc::new(fs::read_to_string(ensure_real_path(filename))?)
        } else {
            match formatted_result.original_text() {
                Some(original_snippet) => Rc::new(original_snippet.to_owned()),
                None => match parse_sess.and_then(|sess| sess.get_original_snippet(filename)) {
                    Some(ori) => ori,
                    None => Rc::new(fs::read_to_string(ensure_real_path(filename))?),
                },
            }
        };

    let formatted_file = FormattedFile {
        filename,
        original_text: original_text.as_str(),
        formatted_text: formatted_result.formatted_text(),
    };

    emitter.emit_formatted_file(out, formatted_file)
}

fn create_emitter(emitter_config: EmitterConfig) -> Box<dyn Emitter> {
    match emitter_config.emit_mode {
        EmitMode::Files => Box::new(FilesEmitter::new(emitter_config)),
        EmitMode::Stdout => Box::new(StdoutEmitter::new(emitter_config)),
        EmitMode::Json => Box::new(JsonEmitter::default()),
        EmitMode::ModifiedLines => Box::new(ModifiedLinesEmitter::default()),
        EmitMode::Checkstyle => Box::new(CheckstyleEmitter::default()),
        EmitMode::Diff => Box::new(DiffEmitter::new(emitter_config)),
    }
}
