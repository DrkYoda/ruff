use std::cmp::Ordering;
use std::fmt::Display;
use std::fs::File;
use std::io::{BufReader, BufWriter, Cursor, Read, Seek, SeekFrom, Write};
use std::iter;
use std::path::Path;

use itertools::Itertools;
use once_cell::sync::OnceCell;
use serde::Serialize;
use serde_json::error::Category;
use uuid::Uuid;

use ruff_diagnostics::Diagnostic;
use ruff_python_parser::lexer::lex;
use ruff_python_parser::Mode;
use ruff_source_file::{NewlineWithTrailingNewline, UniversalNewlineIterator};
use ruff_text_size::{TextRange, TextSize};

use crate::autofix::source_map::{SourceMap, SourceMarker};
use crate::jupyter::index::NotebookIndex;
use crate::jupyter::schema::{Cell, RawNotebook, SortAlphabetically, SourceValue};
use crate::rules::pycodestyle::rules::SyntaxError;
use crate::IOError;

pub const JUPYTER_NOTEBOOK_EXT: &str = "ipynb";

/// Run round-trip source code generation on a given Jupyter notebook file path.
pub fn round_trip(path: &Path) -> anyhow::Result<String> {
    let mut notebook = Notebook::from_path(path).map_err(|err| {
        anyhow::anyhow!(
            "Failed to read notebook file `{}`: {:?}",
            path.display(),
            err
        )
    })?;
    let code = notebook.source_code().to_string();
    notebook.update_cell_content(&code);
    let mut writer = Vec::new();
    notebook.write_inner(&mut writer)?;
    Ok(String::from_utf8(writer)?)
}

impl Display for SourceValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceValue::String(string) => f.write_str(string),
            SourceValue::StringArray(string_array) => {
                for string in string_array {
                    f.write_str(string)?;
                }
                Ok(())
            }
        }
    }
}

impl Cell {
    /// Return the [`SourceValue`] of the cell.
    fn source(&self) -> &SourceValue {
        match self {
            Cell::Code(cell) => &cell.source,
            Cell::Markdown(cell) => &cell.source,
            Cell::Raw(cell) => &cell.source,
        }
    }

    /// Update the [`SourceValue`] of the cell.
    fn set_source(&mut self, source: SourceValue) {
        match self {
            Cell::Code(cell) => cell.source = source,
            Cell::Markdown(cell) => cell.source = source,
            Cell::Raw(cell) => cell.source = source,
        }
    }

    /// Return `true` if it's a valid code cell.
    ///
    /// A valid code cell is a cell where the cell type is [`Cell::Code`] and the
    /// source doesn't contain a cell magic.
    fn is_valid_code_cell(&self) -> bool {
        let source = match self {
            Cell::Code(cell) => &cell.source,
            _ => return false,
        };
        // Ignore cells containing cell magic as they act on the entire cell
        // as compared to line magic which acts on a single line.
        !match source {
            SourceValue::String(string) => string
                .lines()
                .any(|line| line.trim_start().starts_with("%%")),
            SourceValue::StringArray(string_array) => string_array
                .iter()
                .any(|line| line.trim_start().starts_with("%%")),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notebook {
    /// Python source code of the notebook.
    ///
    /// This is the concatenation of all valid code cells in the notebook
    /// separated by a newline and a trailing newline. The trailing newline
    /// is added to make sure that each cell ends with a newline which will
    /// be removed when updating the cell content.
    source_code: String,
    /// The index of the notebook. This is used to map between the concatenated
    /// source code and the original notebook.
    index: OnceCell<NotebookIndex>,
    /// The raw notebook i.e., the deserialized version of JSON string.
    raw: RawNotebook,
    /// The offsets of each cell in the concatenated source code. This includes
    /// the first and last character offsets as well.
    cell_offsets: Vec<TextSize>,
    /// The cell index of all valid code cells in the notebook.
    valid_code_cells: Vec<u32>,
    /// Flag to indicate if the JSON string of the notebook has a trailing newline.
    trailing_newline: bool,
}

impl Notebook {
    /// Read the Jupyter Notebook from the given [`Path`].
    pub fn from_path(path: &Path) -> Result<Self, Box<Diagnostic>> {
        Self::from_reader(BufReader::new(File::open(path).map_err(|err| {
            Diagnostic::new(
                IOError {
                    message: format!("{err}"),
                },
                TextRange::default(),
            )
        })?))
    }

    /// Read the Jupyter Notebook from its JSON string.
    pub fn from_source_code(source_code: &str) -> Result<Self, Box<Diagnostic>> {
        Self::from_reader(Cursor::new(source_code))
    }

    /// Read a Jupyter Notebook from a [`Read`] implementor.
    ///
    /// See also the black implementation
    /// <https://github.com/psf/black/blob/69ca0a4c7a365c5f5eea519a90980bab72cab764/src/black/__init__.py#L1017-L1046>
    fn from_reader<R>(mut reader: R) -> Result<Self, Box<Diagnostic>>
    where
        R: Read + Seek,
    {
        let trailing_newline = reader.seek(SeekFrom::End(-1)).is_ok_and(|_| {
            let mut buf = [0; 1];
            reader.read_exact(&mut buf).is_ok_and(|_| buf[0] == b'\n')
        });
        reader.rewind().map_err(|err| {
            Diagnostic::new(
                IOError {
                    message: format!("{err}"),
                },
                TextRange::default(),
            )
        })?;
        let mut raw_notebook: RawNotebook = match serde_json::from_reader(reader.by_ref()) {
            Ok(notebook) => notebook,
            Err(err) => {
                // Translate the error into a diagnostic
                return Err(Box::new({
                    match err.classify() {
                        Category::Io => Diagnostic::new(
                            IOError {
                                message: format!("{err}"),
                            },
                            TextRange::default(),
                        ),
                        Category::Syntax | Category::Eof => {
                            // Maybe someone saved the python sources (those with the `# %%` separator)
                            // as jupyter notebook instead. Let's help them.
                            let mut contents = String::new();
                            reader
                                .rewind()
                                .and_then(|_| reader.read_to_string(&mut contents))
                                .map_err(|err| {
                                    Diagnostic::new(
                                        IOError {
                                            message: format!("{err}"),
                                        },
                                        TextRange::default(),
                                    )
                                })?;

                            // Check if tokenizing was successful and the file is non-empty
                            if lex(&contents, Mode::Module).any(|result| result.is_err()) {
                                Diagnostic::new(
                                    SyntaxError {
                                        message: format!(
                                            "A Jupyter Notebook (.{JUPYTER_NOTEBOOK_EXT}) must internally be JSON, \
                                but this file isn't valid JSON: {err}"
                                        ),
                                    },
                                    TextRange::default(),
                                )
                            } else {
                                Diagnostic::new(
                                    SyntaxError {
                                        message: format!(
                                            "Expected a Jupyter Notebook (.{JUPYTER_NOTEBOOK_EXT}), \
                                    which must be internally stored as JSON, \
                                    but found a Python source file: {err}"
                                        ),
                                    },
                                    TextRange::default(),
                                )
                            }
                        }
                        Category::Data => {
                            // We could try to read the schema version here but if this fails it's
                            // a bug anyway
                            Diagnostic::new(
                                SyntaxError {
                                    message: format!(
                                        "This file does not match the schema expected of Jupyter Notebooks: {err}"
                                    ),
                                },
                                TextRange::default(),
                            )
                        }
                    }
                }));
            }
        };

        // v4 is what everybody uses
        if raw_notebook.nbformat != 4 {
            // bail because we should have already failed at the json schema stage
            return Err(Box::new(Diagnostic::new(
                SyntaxError {
                    message: format!(
                        "Expected Jupyter Notebook format 4, found {}",
                        raw_notebook.nbformat
                    ),
                },
                TextRange::default(),
            )));
        }

        let valid_code_cells = raw_notebook
            .cells
            .iter()
            .enumerate()
            .filter(|(_, cell)| cell.is_valid_code_cell())
            .map(|(idx, _)| u32::try_from(idx).unwrap())
            .collect::<Vec<_>>();

        let mut contents = Vec::with_capacity(valid_code_cells.len());
        let mut current_offset = TextSize::from(0);
        let mut cell_offsets = Vec::with_capacity(valid_code_cells.len());
        cell_offsets.push(TextSize::from(0));

        for &idx in &valid_code_cells {
            let cell_contents = match &raw_notebook.cells[idx as usize].source() {
                SourceValue::String(string) => string.clone(),
                SourceValue::StringArray(string_array) => string_array.join(""),
            };
            current_offset += TextSize::of(&cell_contents) + TextSize::new(1);
            contents.push(cell_contents);
            cell_offsets.push(current_offset);
        }

        // Add cell ids to 4.5+ notebooks if they are missing
        // https://github.com/astral-sh/ruff/issues/6834
        // https://github.com/jupyter/enhancement-proposals/blob/master/62-cell-id/cell-id.md#required-field
        if raw_notebook.nbformat == 4 && raw_notebook.nbformat_minor >= 5 {
            for cell in &mut raw_notebook.cells {
                let id = match cell {
                    Cell::Code(cell) => &mut cell.id,
                    Cell::Markdown(cell) => &mut cell.id,
                    Cell::Raw(cell) => &mut cell.id,
                };
                if id.is_none() {
                    // https://github.com/jupyter/enhancement-proposals/blob/master/62-cell-id/cell-id.md#questions
                    *id = Some(Uuid::new_v4().to_string());
                }
            }
        }

        Ok(Self {
            raw: raw_notebook,
            index: OnceCell::new(),
            // The additional newline at the end is to maintain consistency for
            // all cells. These newlines will be removed before updating the
            // source code with the transformed content. Refer `update_cell_content`.
            source_code: contents.join("\n") + "\n",
            cell_offsets,
            valid_code_cells,
            trailing_newline,
        })
    }

    /// Update the cell offsets as per the given [`SourceMap`].
    fn update_cell_offsets(&mut self, source_map: &SourceMap) {
        // When there are multiple cells without any edits, the offsets of those
        // cells will be updated using the same marker. So, we can keep track of
        // the last marker used to update the offsets and check if it's still
        // the closest marker to the current offset.
        let mut last_marker: Option<&SourceMarker> = None;

        // The first offset is always going to be at 0, so skip it.
        for offset in self.cell_offsets.iter_mut().skip(1).rev() {
            let closest_marker = match last_marker {
                Some(marker) if marker.source <= *offset => marker,
                _ => {
                    let Some(marker) = source_map
                        .markers()
                        .iter()
                        .rev()
                        .find(|m| m.source <= *offset)
                    else {
                        // There are no markers above the current offset, so we can
                        // stop here.
                        break;
                    };
                    last_marker = Some(marker);
                    marker
                }
            };

            match closest_marker.source.cmp(&closest_marker.dest) {
                Ordering::Less => *offset += closest_marker.dest - closest_marker.source,
                Ordering::Greater => *offset -= closest_marker.source - closest_marker.dest,
                Ordering::Equal => (),
            }
        }
    }

    /// Update the cell contents with the transformed content.
    ///
    /// ## Panics
    ///
    /// Panics if the transformed content is out of bounds for any cell. This
    /// can happen only if the cell offsets were not updated before calling
    /// this method or the offsets were updated incorrectly.
    fn update_cell_content(&mut self, transformed: &str) {
        for (&idx, (start, end)) in self
            .valid_code_cells
            .iter()
            .zip(self.cell_offsets.iter().tuple_windows::<(_, _)>())
        {
            let cell_content = transformed
                .get(start.to_usize()..end.to_usize())
                .unwrap_or_else(|| {
                    panic!(
                        "Transformed content out of bounds ({start:?}..{end:?}) for cell at {idx:?}"
                    );
                });
            self.raw.cells[idx as usize].set_source(SourceValue::StringArray(
                UniversalNewlineIterator::from(
                    // We only need to strip the trailing newline which we added
                    // while concatenating the cell contents.
                    cell_content.strip_suffix('\n').unwrap_or(cell_content),
                )
                .map(|line| line.as_full_str().to_string())
                .collect::<Vec<_>>(),
            ));
        }
    }

    /// Build and return the [`JupyterIndex`].
    ///
    /// ## Notes
    ///
    /// Empty cells don't have any newlines, but there's a single visible line
    /// in the UI. That single line needs to be accounted for.
    ///
    /// In case of [`SourceValue::StringArray`], newlines are part of the strings.
    /// So, to get the actual count of lines, we need to check for any trailing
    /// newline for the last line.
    ///
    /// For example, consider the following cell:
    /// ```python
    /// [
    ///    "import os\n",
    ///    "import sys\n",
    /// ]
    /// ```
    ///
    /// Here, the array suggests that there are two lines, but the actual number
    /// of lines visible in the UI is three. The same goes for [`SourceValue::String`]
    /// where we need to check for the trailing newline.
    ///
    /// The index building is expensive as it needs to go through the content of
    /// every valid code cell.
    fn build_index(&self) -> NotebookIndex {
        let mut row_to_cell = vec![0];
        let mut row_to_row_in_cell = vec![0];

        for &idx in &self.valid_code_cells {
            let line_count = match &self.raw.cells[idx as usize].source() {
                SourceValue::String(string) => {
                    if string.is_empty() {
                        1
                    } else {
                        u32::try_from(NewlineWithTrailingNewline::from(string).count()).unwrap()
                    }
                }
                SourceValue::StringArray(string_array) => {
                    if string_array.is_empty() {
                        1
                    } else {
                        let trailing_newline =
                            usize::from(string_array.last().is_some_and(|s| s.ends_with('\n')));
                        u32::try_from(string_array.len() + trailing_newline).unwrap()
                    }
                }
            };
            row_to_cell.extend(iter::repeat(idx + 1).take(line_count as usize));
            row_to_row_in_cell.extend(1..=line_count);
        }

        NotebookIndex {
            row_to_cell,
            row_to_row_in_cell,
        }
    }

    /// Return the notebook content.
    ///
    /// This is the concatenation of all Python code cells.
    pub fn source_code(&self) -> &str {
        &self.source_code
    }

    /// Return the Jupyter notebook index.
    ///
    /// The index is built only once when required. This is only used to
    /// report diagnostics, so by that time all of the autofixes must have
    /// been applied if `--fix` was passed.
    pub(crate) fn index(&self) -> &NotebookIndex {
        self.index.get_or_init(|| self.build_index())
    }

    /// Return the cell offsets for the concatenated source code corresponding
    /// the Jupyter notebook.
    pub(crate) fn cell_offsets(&self) -> &[TextSize] {
        &self.cell_offsets
    }

    /// Update the notebook with the given sourcemap and transformed content.
    pub(crate) fn update(&mut self, source_map: &SourceMap, transformed: String) {
        // Cell offsets must be updated before updating the cell content as
        // it depends on the offsets to extract the cell content.
        self.index.take();
        self.update_cell_offsets(source_map);
        self.update_cell_content(&transformed);
        self.source_code = transformed;
    }

    /// Return a slice of [`Cell`] in the Jupyter notebook.
    pub fn cells(&self) -> &[Cell] {
        &self.raw.cells
    }

    /// Return `true` if the notebook is a Python notebook, `false` otherwise.
    pub fn is_python_notebook(&self) -> bool {
        self.raw
            .metadata
            .language_info
            .as_ref()
            .map_or(true, |language| language.name == "python")
    }

    fn write_inner(&self, writer: &mut impl Write) -> anyhow::Result<()> {
        // https://github.com/psf/black/blob/69ca0a4c7a365c5f5eea519a90980bab72cab764/src/black/__init__.py#LL1041
        let formatter = serde_json::ser::PrettyFormatter::with_indent(b" ");
        let mut serializer = serde_json::Serializer::with_formatter(writer, formatter);
        SortAlphabetically(&self.raw).serialize(&mut serializer)?;
        if self.trailing_newline {
            writeln!(serializer.into_inner())?;
        }
        Ok(())
    }

    /// Write back with an indent of 1, just like black
    pub fn write(&self, path: &Path) -> anyhow::Result<()> {
        let mut writer = BufWriter::new(File::create(path)?);
        self.write_inner(&mut writer)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use anyhow::Result;
    use test_case::test_case;

    use crate::jupyter::index::NotebookIndex;
    use crate::jupyter::schema::Cell;
    use crate::jupyter::Notebook;
    use crate::registry::Rule;
    use crate::source_kind::SourceKind;
    use crate::test::{
        read_jupyter_notebook, test_contents, test_notebook_path, test_resource_path,
        TestedNotebook,
    };
    use crate::{assert_messages, settings};

    /// Read a Jupyter cell from the `resources/test/fixtures/jupyter/cell` directory.
    fn read_jupyter_cell(path: impl AsRef<Path>) -> Result<Cell> {
        let path = test_resource_path("fixtures/jupyter/cell").join(path);
        let source_code = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&source_code)?)
    }

    #[test]
    fn test_valid() {
        assert!(read_jupyter_notebook(Path::new("valid.ipynb")).is_ok());
    }

    #[test]
    fn test_r() {
        // We can load this, it will be filtered out later
        assert!(read_jupyter_notebook(Path::new("R.ipynb")).is_ok());
    }

    #[test]
    fn test_invalid() {
        let path = Path::new("resources/test/fixtures/jupyter/invalid_extension.ipynb");
        assert_eq!(
            Notebook::from_path(path).unwrap_err().kind.body,
            "SyntaxError: Expected a Jupyter Notebook (.ipynb), \
            which must be internally stored as JSON, \
            but found a Python source file: \
            expected value at line 1 column 1"
        );
        let path = Path::new("resources/test/fixtures/jupyter/not_json.ipynb");
        assert_eq!(
            Notebook::from_path(path).unwrap_err().kind.body,
            "SyntaxError: A Jupyter Notebook (.ipynb) must internally be JSON, \
            but this file isn't valid JSON: \
            expected value at line 1 column 1"
        );
        let path = Path::new("resources/test/fixtures/jupyter/wrong_schema.ipynb");
        assert_eq!(
            Notebook::from_path(path).unwrap_err().kind.body,
            "SyntaxError: This file does not match the schema expected of Jupyter Notebooks: \
            missing field `cells` at line 1 column 2"
        );
    }

    #[test_case(Path::new("markdown.json"), false; "markdown")]
    #[test_case(Path::new("only_magic.json"), true; "only_magic")]
    #[test_case(Path::new("code_and_magic.json"), true; "code_and_magic")]
    #[test_case(Path::new("only_code.json"), true; "only_code")]
    #[test_case(Path::new("cell_magic.json"), false; "cell_magic")]
    fn test_is_valid_code_cell(path: &Path, expected: bool) -> Result<()> {
        assert_eq!(read_jupyter_cell(path)?.is_valid_code_cell(), expected);
        Ok(())
    }

    #[test]
    fn test_concat_notebook() -> Result<()> {
        let notebook = read_jupyter_notebook(Path::new("valid.ipynb"))?;
        assert_eq!(
            notebook.source_code,
            r#"def unused_variable():
    x = 1
    y = 2
    print(f"cell one: {y}")

unused_variable()
def mutable_argument(z=set()):
  print(f"cell two: {z}")

mutable_argument()




print("after empty cells")
"#
        );
        assert_eq!(
            notebook.index(),
            &NotebookIndex {
                row_to_cell: vec![0, 1, 1, 1, 1, 1, 1, 3, 3, 3, 3, 3, 5, 7, 7, 8],
                row_to_row_in_cell: vec![0, 1, 2, 3, 4, 5, 6, 1, 2, 3, 4, 5, 1, 1, 2, 1],
            }
        );
        assert_eq!(
            notebook.cell_offsets(),
            &[
                0.into(),
                90.into(),
                168.into(),
                169.into(),
                171.into(),
                198.into()
            ]
        );
        Ok(())
    }

    #[test]
    fn test_import_sorting() -> Result<()> {
        let path = "isort.ipynb".to_string();
        let TestedNotebook {
            messages,
            source_notebook,
            ..
        } = test_notebook_path(
            &path,
            Path::new("isort_expected.ipynb"),
            &settings::Settings::for_rule(Rule::UnsortedImports),
        )?;
        assert_messages!(messages, path, source_notebook);
        Ok(())
    }

    #[test]
    fn test_ipy_escape_command() -> Result<()> {
        let path = "ipy_escape_command.ipynb".to_string();
        let TestedNotebook {
            messages,
            source_notebook,
            ..
        } = test_notebook_path(
            &path,
            Path::new("ipy_escape_command_expected.ipynb"),
            &settings::Settings::for_rule(Rule::UnusedImport),
        )?;
        assert_messages!(messages, path, source_notebook);
        Ok(())
    }

    #[test]
    fn test_unused_variable() -> Result<()> {
        let path = "unused_variable.ipynb".to_string();
        let TestedNotebook {
            messages,
            source_notebook,
            ..
        } = test_notebook_path(
            &path,
            Path::new("unused_variable_expected.ipynb"),
            &settings::Settings::for_rule(Rule::UnusedVariable),
        )?;
        assert_messages!(messages, path, source_notebook);
        Ok(())
    }

    #[test]
    fn test_json_consistency() -> Result<()> {
        let path = "before_fix.ipynb".to_string();
        let TestedNotebook {
            linted_notebook: fixed_notebook,
            ..
        } = test_notebook_path(
            path,
            Path::new("after_fix.ipynb"),
            &settings::Settings::for_rule(Rule::UnusedImport),
        )?;
        let mut writer = Vec::new();
        fixed_notebook.write_inner(&mut writer)?;
        let actual = String::from_utf8(writer)?;
        let expected =
            std::fs::read_to_string(test_resource_path("fixtures/jupyter/after_fix.ipynb"))?;
        assert_eq!(actual, expected);
        Ok(())
    }

    #[test_case(Path::new("before_fix.ipynb"), true; "trailing_newline")]
    #[test_case(Path::new("no_trailing_newline.ipynb"), false; "no_trailing_newline")]
    fn test_trailing_newline(path: &Path, trailing_newline: bool) -> Result<()> {
        let notebook = read_jupyter_notebook(path)?;
        assert_eq!(notebook.trailing_newline, trailing_newline);

        let mut writer = Vec::new();
        notebook.write_inner(&mut writer)?;
        let string = String::from_utf8(writer)?;
        assert_eq!(string.ends_with('\n'), trailing_newline);

        Ok(())
    }

    // Version <4.5, don't emit cell ids
    #[test_case(Path::new("no_cell_id.ipynb"), false; "no_cell_id")]
    // Version 4.5, cell ids are missing and need to be added
    #[test_case(Path::new("add_missing_cell_id.ipynb"), true; "add_missing_cell_id")]
    fn test_cell_id(path: &Path, has_id: bool) -> Result<()> {
        let source_notebook = read_jupyter_notebook(path)?;
        let source_kind = SourceKind::IpyNotebook(source_notebook);
        let (_, transformed) = test_contents(
            &source_kind,
            path,
            &settings::Settings::for_rule(Rule::UnusedImport),
        );
        let linted_notebook = transformed.into_owned().expect_ipy_notebook();
        let mut writer = Vec::new();
        linted_notebook.write_inner(&mut writer)?;
        let actual = String::from_utf8(writer)?;
        if has_id {
            assert!(actual.contains(r#""id": ""#));
        } else {
            assert!(!actual.contains(r#""id":"#));
        }
        Ok(())
    }
}
