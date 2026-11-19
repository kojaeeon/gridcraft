//! ODF worksheet data plus an optional GridCraft workbook part. The checksum binds the
//! private part to the standard worksheet data so edits made in another app take precedence.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use gridcraft_core::{CellRef, DateSystem, RangeRef, Value, addr::col_to_letters};
use gridcraft_formula::{Anchor, BinOp, Expr, RefKind, Reference, SheetSel, UnOp};
use gridcraft_model::{Cell, Sheet, Workbook};
use sha2::{Digest, Sha256};

use crate::IoError;
use crate::package::{MAX_PART, Package};
use crate::xml::{esc, esc_attr};

const CONTENT: &str = "content.xml";
const WORKBOOK: &str = "GridCraft/workbook.bin";
const CHECKSUM_LEN: usize = 32;
const MAX_MERGE_ROWS: u64 = 1_000_000;

pub(crate) fn read_snapshot(package: &mut Package<'_>, content: &[u8]) -> Option<Vec<u8>> {
    // An external editor can add styling or change settings without touching content.xml.
    if ["styles.xml", "settings.xml", "meta.xml"].iter().any(|name| package.has(name)) {
        return None;
    }
    let mut snapshot = package.read_optional_limited(WORKBOOK, MAX_PART).ok()??;
    let hash = Sha256::digest(content);
    if snapshot.get(..CHECKSUM_LEN)? != &hash[..] {
        return None;
    }
    snapshot.drain(..CHECKSUM_LEN);
    Some(snapshot)
}

/// Writes an ODF 1.3 spreadsheet with typed cells, merges and common OpenFormula expressions.
/// The caller supplies its XLSX-family serialization of the same workbook for GridCraft features
/// that don't have a mapping to OpenDocument.
pub fn write_ods(wb: &Workbook, mut snapshot: Vec<u8>) -> Result<Vec<u8>, IoError> {
    if snapshot.len() as u64 > MAX_PART - CHECKSUM_LEN as u64 {
        return Err(IoError::TooLarge("the embedded ODS workbook exceeds the part limit".into()));
    }
    let mut content = String::from(concat!(
        r#"<?xml version="1.0" encoding="UTF-8"?>"#,
        r#"<office:document-content office:version="1.3""#,
        r#" xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0""#,
        r#" xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0""#,
        r#" xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0""#,
        r#" xmlns:of="urn:oasis:names:tc:opendocument:xmlns:of:1.2">"#,
        "<office:body><office:spreadsheet>",
    ));
    let null_date = if wb.date_system == DateSystem::D1904 { "1904-01-01" } else { "1899-12-30" };
    let _ = write!(content, r#"<table:calculation-settings><table:null-date table:date-value="{null_date}"/></table:calculation-settings>"#);
    let mut merge_rows_left = MAX_MERGE_ROWS;
    for sheet in &wb.sheets {
        write_sheet(&mut content, sheet, &mut merge_rows_left)?;
    }
    content.push_str("</office:spreadsheet></office:body></office:document-content>");
    check_content_size(&content)?;

    let mut package = craft_odf::PackageWriter::new(craft_odf::mime::SPREADSHEET);
    let hash = Sha256::digest(content.as_bytes());
    snapshot.splice(..0, hash);
    package.file(CONTENT, "text/xml", content.as_bytes());
    package.stored(WORKBOOK, "application/octet-stream", &snapshot);
    package.finish().map_err(|e| IoError::Zip(e.to_string()))
}

fn check_content_size(xml: &str) -> Result<(), IoError> {
    if xml.len() as u64 > MAX_PART { Err(IoError::TooLarge("ODS content.xml exceeds the part limit".into())) } else { Ok(()) }
}

enum Entry<'a> {
    Cell(Option<&'a Cell>, &'a Value, Option<RangeRef>),
    Covered(u32),
}

fn write_sheet(xml: &mut String, sheet: &Sheet, merge_rows_left: &mut u64) -> Result<(), IoError> {
    let mut entries = BTreeMap::new();
    for (at, cell) in sheet.cells.iter() {
        if at.is_valid() && cell.has_content() {
            entries.insert(at, Entry::Cell(Some(cell), &cell.value, None));
        }
    }
    for (&at, value) in &sheet.spill {
        if at.is_valid() {
            entries.entry(at).or_insert(Entry::Cell(None, value, None));
        }
    }
    for &range in &sheet.merges {
        if !range.start.is_valid() || !range.end.is_valid() || range.start.row > range.end.row || range.start.col > range.end.col {
            continue;
        }
        *merge_rows_left = merge_rows_left
            .checked_sub(u64::from(range.height()))
            .ok_or_else(|| IoError::TooLarge("ODS has too many rows with merged cells".into()))?;
        let cell = sheet.cell(range.start);
        entries.insert(range.start, Entry::Cell(cell, cell.map_or(&Value::Empty, |c| &c.value), Some(range)));
        if range.width() > 1 {
            entries.insert(CellRef::new(range.start.row, range.start.col + 1), Entry::Covered(range.width() - 1));
        }
        for row in range.start.row + 1..=range.end.row {
            entries.insert(CellRef::new(row, range.start.col), Entry::Covered(range.width()));
        }
    }

    let _ = write!(xml, r#"<table:table table:name="{}">"#, esc_attr(&sheet.name));
    let mut row = None;
    let mut col = 0;
    for (at, entry) in entries {
        if row != Some(at.row) {
            if row.is_some() {
                xml.push_str("</table:table-row>");
            }
            let gap = at.row - row.map_or(0, |r| r + 1);
            if gap > 0 {
                let _ = write!(xml, r#"<table:table-row table:number-rows-repeated="{gap}"><table:table-cell/></table:table-row>"#);
            }
            row = Some(at.row);
            col = 0;
            xml.push_str("<table:table-row>");
        }
        if at.col < col {
            continue;
        }
        if at.col > col {
            let gap = at.col - col;
            let _ = write!(xml, r#"<table:table-cell table:number-columns-repeated="{gap}"/>"#);
        }
        col = at.col
            + match entry {
                Entry::Cell(cell, value, merge) => {
                    write_cell(xml, cell, value, merge);
                    1
                }
                Entry::Covered(width) => {
                    let _ = write!(xml, r#"<table:covered-table-cell table:number-columns-repeated="{width}"/>"#);
                    width
                }
            };
        check_content_size(xml)?;
    }
    if row.is_some() {
        xml.push_str("</table:table-row>");
    } else {
        xml.push_str("<table:table-row><table:table-cell/></table:table-row>");
    }
    xml.push_str("</table:table>");
    Ok(())
}

fn write_cell(xml: &mut String, cell: Option<&Cell>, value: &Value, merge: Option<RangeRef>) {
    xml.push_str("<table:table-cell");
    if let Some(range) = merge {
        let _ = write!(xml, r#" table:number-columns-spanned="{}" table:number-rows-spanned="{}""#, range.width(), range.height());
    }
    let value = value.scalar();
    if let Some(f) = cell.and_then(|c| c.formula.as_deref()).filter(|f| !f.dynamic && f.array.is_none())
        && let Some(formula) = f
            .with_expr(|e| {
                let mut out = String::new();
                write_formula(&mut out, e, 64)?;
                Some(out)
            })
            .flatten()
    {
        let _ = write!(xml, r#" table:formula="of:={}""#, esc_attr(&formula));
    }

    match value {
        Value::Empty => {
            xml.push_str("/>");
            return;
        }
        Value::Number(n) if n.is_finite() => {
            let _ = write!(xml, r#" office:value-type="float" office:value="{n}""#);
        }
        Value::Bool(b) => {
            let _ = write!(xml, r#" office:value-type="boolean" office:boolean-value="{b}""#);
        }
        _ => xml.push_str(r#" office:value-type="string""#),
    }
    xml.push_str("><text:p>");
    match value {
        Value::Text(text) => write_text(xml, &text),
        other => xml.push_str(&esc(&other.display())),
    }
    xml.push_str("</text:p></table:table-cell>");
}

fn write_text(xml: &mut String, text: &str) {
    let mut start = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        match ch {
            ' ' | '\t' | '\r' | '\n' => {
                xml.push_str(&esc(&text[start..offset]));
                start = offset + 1;
                match ch {
                    ' ' => {
                        while chars.next_if(|(_, next)| *next == ' ').is_some() {
                            start += 1;
                        }
                        let count = start - offset;
                        let _ = write!(xml, r#"<text:s text:c="{count}"/>"#);
                    }
                    '\t' => xml.push_str("<text:tab/>"),
                    _ => xml.push_str("<text:line-break/>"),
                }
            }
            _ => {}
        }
    }
    xml.push_str(&esc(&text[start..]));
}

fn write_anchor(out: &mut String, at: Anchor) {
    let col = col_to_letters(at.col);
    let _ = write!(out, ".{}{col}{}{}", if at.col_abs { "$" } else { "" }, if at.row_abs { "$" } else { "" }, at.row + 1);
}

/// Translate only expressions with matching OpenFormula semantics. Everything else keeps its
/// cached value in the standard sheet and its formula in the GridCraft workbook part.
fn write_formula(out: &mut String, expr: &Expr, depth: u8) -> Option<()> {
    let depth = depth.checked_sub(1)?;
    match expr {
        Expr::Number(n) if n.is_finite() => write!(out, "{n}").ok()?,
        Expr::Text(text) => write!(out, "\"{}\"", text.replace('"', "\"\"")).ok()?,
        Expr::Bool(b) => out.push_str(if *b { "TRUE()" } else { "FALSE()" }),
        Expr::Ref(Reference { sheet, kind }) => {
            out.push('[');
            match sheet {
                SheetSel::Current => {}
                SheetSel::Named(name) => write!(out, "$'{}'", name.replace('\'', "''")).ok()?,
                SheetSel::Span(..) => return None,
            }
            match kind {
                RefKind::Cell(a) => write_anchor(out, *a),
                RefKind::Range(a, b) => {
                    write_anchor(out, *a);
                    out.push(':');
                    write_anchor(out, *b);
                }
                _ => return None,
            }
            out.push(']');
        }
        Expr::Unary(UnOp::Neg | UnOp::Plus | UnOp::Percent, arg) => {
            out.push('(');
            if let Expr::Unary(UnOp::Neg | UnOp::Plus, _) = expr {
                out.push(if matches!(expr, Expr::Unary(UnOp::Neg, _)) { '-' } else { '+' });
            }
            write_formula(out, arg, depth)?;
            if matches!(expr, Expr::Unary(UnOp::Percent, _)) {
                out.push('%');
            }
            out.push(')');
        }
        Expr::Binary(op, a, b) if !matches!(op, BinOp::Range | BinOp::Union | BinOp::Intersect) => {
            out.push('(');
            write_formula(out, a, depth)?;
            out.push_str(op.symbol());
            write_formula(out, b, depth)?;
            out.push(')');
        }
        Expr::Call(name, args)
            if matches!(
                name.as_str(),
                "SUM"
                    | "AVERAGE"
                    | "MIN"
                    | "MAX"
                    | "COUNT"
                    | "COUNTA"
                    | "IF"
                    | "IFERROR"
                    | "AND"
                    | "OR"
                    | "NOT"
                    | "ABS"
                    | "ROUND"
                    | "ROUNDUP"
                    | "ROUNDDOWN"
                    | "INT"
                    | "MOD"
                    | "SQRT"
                    | "LEN"
                    | "LEFT"
                    | "RIGHT"
                    | "MID"
            ) =>
        {
            out.push_str(name);
            out.push('(');
            for (i, arg) in args.iter().enumerate() {
                if i > 0 {
                    out.push(';');
                }
                write_formula(out, arg, depth)?;
            }
            out.push(')');
        }
        Expr::Paren(e) => write_formula(out, e, depth)?,
        _ => return None,
    }
    Some(())
}
