//! Reads cell text and links from an `.xlsx` file. It covers what the tracker import needs and no more.

use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Seek};

use roxmltree::{Document, Node};
use zip::ZipArchive;

const RELATIONSHIPS: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

pub struct Sheet {
    pub name: String,
    pub rows: Vec<Row>,
}

pub struct Row {
    /// The row number as the spreadsheet shows it, starting at 1.
    pub number: u32,
    cells: HashMap<u32, Cell>,
}

impl Row {
    /// `column` counts from 0, so column A is 0.
    pub fn cell(&self, column: u32) -> Option<&Cell> {
        self.cells.get(&column)
    }

    pub fn cells(&self) -> impl Iterator<Item = (u32, &Cell)> {
        self.cells.iter().map(|(&column, cell)| (column, cell))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Cell {
    /// A number cell holds the number as the file wrote it, such as `46267.0`.
    pub text: String,
    pub link: Option<String>,
}

#[derive(Debug)]
pub struct XlsxError(String);

impl fmt::Display for XlsxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for XlsxError {}

pub fn read(file: impl Read + Seek) -> Result<Vec<Sheet>, XlsxError> {
    let mut archive = ZipArchive::new(file).map_err(|error| XlsxError(format!("not an .xlsx file: {error}")))?;
    let shared = match part(&mut archive, "xl/sharedStrings.xml")? {
        Some(xml) => shared_strings(&parse("xl/sharedStrings.xml", &xml)?),
        None => Vec::new(),
    };
    let workbook = required_part(&mut archive, "xl/workbook.xml")?;
    let targets = relationships(&mut archive, "xl/_rels/workbook.xml.rels")?;

    let mut sheets = Vec::new();
    for sheet in named(parse("xl/workbook.xml", &workbook)?.root(), "sheet") {
        let name = sheet.attribute("name").unwrap_or_default().to_owned();
        let target = sheet
            .attribute((RELATIONSHIPS, "id"))
            .and_then(|id| targets.get(id))
            .ok_or_else(|| XlsxError(format!("the workbook does not say where sheet {name:?} is stored")))?;
        let path = match target.strip_prefix('/') {
            Some(absolute) => absolute.to_owned(),
            None => format!("xl/{target}"),
        };
        let links = match path.rsplit_once('/') {
            Some((folder, file)) => relationships(&mut archive, &format!("{folder}/_rels/{file}.rels"))?,
            None => HashMap::new(),
        };
        let xml = required_part(&mut archive, &path)?;
        let rows = rows(&parse(&path, &xml)?, &shared, &links)?;
        sheets.push(Sheet { name, rows });
    }
    Ok(sheets)
}

fn part<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Result<Option<String>, XlsxError> {
    let mut file = match archive.by_name(name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Ok(None),
        Err(error) => return Err(XlsxError(format!("could not open {name}: {error}"))),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|error| XlsxError(format!("could not read {name}: {error}")))?;
    Ok(Some(text))
}

fn required_part<R: Read + Seek>(archive: &mut ZipArchive<R>, name: &str) -> Result<String, XlsxError> {
    part(archive, name)?.ok_or_else(|| XlsxError(format!("the file has no {name}")))
}

fn parse<'a>(name: &str, xml: &'a str) -> Result<Document<'a>, XlsxError> {
    Document::parse(xml).map_err(|error| XlsxError(format!("{name} is not valid XML: {error}")))
}

fn named<'a, 'input>(from: Node<'a, 'input>, tag: &'static str) -> impl Iterator<Item = Node<'a, 'input>> {
    from.descendants().filter(move |node| node.tag_name().name() == tag)
}

fn text_runs(node: Node<'_, '_>) -> String {
    named(node, "t").filter_map(|run| run.text()).collect()
}

fn shared_strings(document: &Document<'_>) -> Vec<String> {
    named(document.root(), "si").map(text_runs).collect()
}

/// Relationship id to target. A missing relationships part means there are none.
fn relationships<R: Read + Seek>(
    archive: &mut ZipArchive<R>,
    name: &str,
) -> Result<HashMap<String, String>, XlsxError> {
    let Some(xml) = part(archive, name)? else {
        return Ok(HashMap::new());
    };
    Ok(named(parse(name, &xml)?.root(), "Relationship")
        .filter_map(|relationship| {
            Some((
                relationship.attribute("Id")?.to_owned(),
                relationship.attribute("Target")?.to_owned(),
            ))
        })
        .collect())
}

fn rows(
    document: &Document<'_>,
    shared: &[String],
    link_targets: &HashMap<String, String>,
) -> Result<Vec<Row>, XlsxError> {
    let cell_links: HashMap<&str, &str> = named(document.root(), "hyperlink")
        .filter_map(|link| {
            let cell = link.attribute("ref")?.split(':').next()?;
            let target = link_targets.get(link.attribute((RELATIONSHIPS, "id"))?)?;
            Some((cell, target.as_str()))
        })
        .collect();

    let mut rows = Vec::new();
    for row in named(document.root(), "row") {
        let number = row
            .attribute("r")
            .and_then(|number| number.parse().ok())
            .ok_or_else(|| XlsxError("a row has no row number".into()))?;
        let mut cells = HashMap::new();
        for cell in named(row, "c") {
            let reference = cell
                .attribute("r")
                .ok_or_else(|| XlsxError(format!("a cell in row {number} has no address")))?;
            let column = column_of(reference).ok_or_else(|| XlsxError(format!("{reference} is not a cell address")))?;
            let value = named(cell, "v")
                .next()
                .and_then(|value| value.text())
                .unwrap_or_default();
            let text = match cell.attribute("t") {
                Some("s") => {
                    let index: usize = value
                        .parse()
                        .map_err(|_| XlsxError(format!("{reference} has a bad shared string index")))?;
                    shared.get(index).cloned().ok_or_else(|| {
                        XlsxError(format!("{reference} points at a shared string that does not exist"))
                    })?
                }
                Some("inlineStr") => text_runs(cell),
                _ => value.to_owned(),
            };
            let formula = named(cell, "f").next().and_then(|formula| formula.text());
            let link = cell_links
                .get(reference)
                .copied()
                .or_else(|| formula.and_then(hyperlink_target))
                .map(str::to_owned);
            if !text.is_empty() || link.is_some() {
                cells.insert(column, Cell { text, link });
            }
        }
        if !cells.is_empty() {
            rows.push(Row { number, cells });
        }
    }
    Ok(rows)
}

/// `"AB12"` is column 27, counting from 0.
fn column_of(reference: &str) -> Option<u32> {
    let letters = reference.trim_end_matches(|c: char| c.is_ascii_digit());
    if letters.is_empty() || letters.len() == reference.len() || !letters.bytes().all(|b| b.is_ascii_uppercase()) {
        return None;
    }
    letters
        .bytes()
        .try_fold(0u32, |column, letter| {
            column.checked_mul(26)?.checked_add(u32::from(letter - b'A') + 1)
        })
        .map(|column| column - 1)
}

/// The address inside a `HYPERLINK("address","label")` formula.
fn hyperlink_target(formula: &str) -> Option<&str> {
    let formula = formula.trim_start_matches('=');
    let open = "HYPERLINK(\"";
    if !formula.get(..open.len())?.eq_ignore_ascii_case(open) {
        return None;
    }
    let rest = &formula[open.len()..];
    rest.find('"').map(|end| &rest[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cell_address_gives_its_column() {
        assert_eq!(column_of("A1"), Some(0));
        assert_eq!(column_of("L2161"), Some(11));
        assert_eq!(column_of("AB12"), Some(27));
        assert_eq!(column_of("12"), None);
        assert_eq!(column_of("A"), None);
    }

    #[test]
    fn a_hyperlink_formula_gives_its_address() {
        assert_eq!(
            hyperlink_target(r#"HYPERLINK("https://jobs.example.com/1","Example Co, Engineer")"#),
            Some("https://jobs.example.com/1")
        );
        assert_eq!(
            hyperlink_target(r#"=hyperlink("https://jobs.example.com/2")"#),
            Some("https://jobs.example.com/2")
        );
        assert_eq!(hyperlink_target(r#"IFERROR(A1,"09/01/2026")"#), None);
    }
}
