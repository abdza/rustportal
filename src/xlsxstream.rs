//! A minimal .xlsx writer that streams rows straight into the zip file, for exports
//! of any size in constant memory. (rust_xlsxwriter's "constant memory" mode still
//! reads each finished sheet back into memory when it saves, which for a 500k-row
//! tracker is gigabytes.)
//!
//! Output: one or more sheets (a new one every SHEET_ROWS rows), a bold header row,
//! numbers as numbers, booleans as booleans, everything else as inline strings.

use anyhow::Result;
use serde_json::Value;
use std::fs::File;
use std::io::{BufWriter, Write};
use zip::write::SimpleFileOptions;

/// Excel's limit is 1,048,576 rows per sheet, header included.
pub const SHEET_ROWS: u32 = 1_048_575;
/// Excel's limit for the text in one cell.
const CELL_CHARS: usize = 32_767;

pub struct XlsxStream {
    zip: zip::ZipWriter<BufWriter<File>>,
    name: String,
    header: Vec<String>,
    sheets: Vec<String>,
    row: u32,
    open: bool,
    line: String,
}

fn opts() -> SimpleFileOptions {
    SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated).large_file(true)
}

fn col_name(mut c: usize) -> String {
    let mut s = vec![];
    c += 1;
    while c > 0 {
        let m = (c - 1) % 26;
        s.push((b'A' + m as u8) as char);
        c = (c - 1) / 26;
    }
    s.iter().rev().collect()
}

/// XML-escape, dropping characters XML 1.0 does not allow.
fn esc_into(out: &mut String, x: &str) {
    for ch in x.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            // a raw CR would be folded into LF by any XML reader, so write it as a reference
            '\r' => out.push_str("&#13;"),
            '\t' | '\n' => out.push(ch),
            c if (c as u32) < 0x20 || c == '\u{FFFE}' || c == '\u{FFFF}' => {}
            c => out.push(c),
        }
    }
}

impl XlsxStream {
    pub fn create(path: &str, sheet_name: &str, header: Vec<String>) -> Result<XlsxStream> {
        let file = BufWriter::with_capacity(1 << 20, File::create(path)?);
        let name: String = sheet_name.chars().filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\')).take(28).collect();
        let name = if name.trim().is_empty() { "Sheet".to_string() } else { name.trim().to_string() };
        Ok(XlsxStream { zip: zip::ZipWriter::new(file), name, header, sheets: vec![], row: 0, open: false, line: String::with_capacity(1 << 16) })
    }

    fn start_sheet(&mut self) -> Result<()> {
        if self.open {
            self.zip.write_all(b"</sheetData></worksheet>")?;
        }
        let n = self.sheets.len() + 1;
        self.sheets.push(if n == 1 { self.name.clone() } else { format!("{} {}", self.name, n) });
        self.zip.start_file(format!("xl/worksheets/sheet{}.xml", n), opts())?;
        self.zip.write_all(br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheetViews><sheetView workbookViewId="0"><pane ySplit="1" topLeftCell="A2" activePane="bottomLeft" state="frozen"/></sheetView></sheetViews><sheetData>"#)?;
        self.open = true;
        self.row = 1;
        let mut l = String::from("<row r=\"1\">");
        for (c, h) in self.header.iter().enumerate() {
            l.push_str(&format!("<c r=\"{}1\" t=\"inlineStr\" s=\"1\"><is><t xml:space=\"preserve\">", col_name(c)));
            esc_into(&mut l, h);
            l.push_str("</t></is></c>");
        }
        l.push_str("</row>");
        self.zip.write_all(l.as_bytes())?;
        Ok(())
    }

    pub fn write_row(&mut self, cells: &[Value]) -> Result<()> {
        if !self.open || self.row > SHEET_ROWS {
            self.start_sheet()?;
        }
        self.row += 1;
        let r = self.row;
        let mut l = std::mem::take(&mut self.line);
        l.clear();
        l.push_str(&format!("<row r=\"{}\">", r));
        for (c, v) in cells.iter().enumerate() {
            match v {
                Value::Null => {}
                Value::Bool(b) => l.push_str(&format!("<c r=\"{}{}\" t=\"b\"><v>{}</v></c>", col_name(c), r, if *b { 1 } else { 0 })),
                Value::Number(n) => match n.as_f64() {
                    Some(f) if f.is_finite() => l.push_str(&format!("<c r=\"{}{}\"><v>{}</v></c>", col_name(c), r, n)),
                    _ => {}
                },
                other => {
                    let text = crate::db::vs(other);
                    if text.is_empty() {
                        continue;
                    }
                    l.push_str(&format!("<c r=\"{}{}\" t=\"inlineStr\"><is><t xml:space=\"preserve\">", col_name(c), r));
                    if text.chars().count() > CELL_CHARS {
                        let cut: String = text.chars().take(CELL_CHARS).collect();
                        esc_into(&mut l, &cut);
                    } else {
                        esc_into(&mut l, &text);
                    }
                    l.push_str("</t></is></c>");
                }
            }
        }
        l.push_str("</row>");
        self.zip.write_all(l.as_bytes())?;
        self.line = l;
        Ok(())
    }

    /// Write the workbook parts and close the file. Returns the number of sheets.
    pub fn finish(mut self) -> Result<usize> {
        if !self.open {
            self.start_sheet()?;
        }
        self.zip.write_all(b"</sheetData></worksheet>")?;
        let o = SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
        let n = self.sheets.len();
        let mut ct = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/><Default Extension="xml" ContentType="application/xml"/><Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/><Override PartName="/xl/styles.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml"/>"#);
        let mut wb = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><sheets>"#);
        let mut rels = String::from(r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">"#);
        for (i, name) in self.sheets.iter().enumerate() {
            let k = i + 1;
            ct.push_str(&format!(r#"<Override PartName="/xl/worksheets/sheet{k}.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>"#));
            let mut nm = String::new();
            esc_into(&mut nm, name);
            wb.push_str(&format!(r#"<sheet name="{nm}" sheetId="{k}" r:id="rId{k}"/>"#));
            rels.push_str(&format!(r#"<Relationship Id="rId{k}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet{k}.xml"/>"#));
        }
        ct.push_str("</Types>");
        wb.push_str("</sheets></workbook>");
        rels.push_str(&format!(r#"<Relationship Id="rId{}" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/styles" Target="styles.xml"/></Relationships>"#, n + 1));
        let parts: [(&str, String); 5] = [
            ("[Content_Types].xml", ct),
            ("_rels/.rels", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/></Relationships>"#.to_string()),
            ("xl/workbook.xml", wb),
            ("xl/_rels/workbook.xml.rels", rels),
            ("xl/styles.xml", r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><fonts count="2"><font><sz val="11"/><name val="Calibri"/><family val="2"/></font><font><b/><sz val="11"/><name val="Calibri"/><family val="2"/></font></fonts><fills count="2"><fill><patternFill patternType="none"/></fill><fill><patternFill patternType="gray125"/></fill></fills><borders count="1"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count="1"><xf numFmtId="0" fontId="0" fillId="0" borderId="0"/></cellStyleXfs><cellXfs count="2"><xf numFmtId="0" fontId="0" fillId="0" borderId="0" xfId="0"/><xf numFmtId="0" fontId="1" fillId="0" borderId="0" xfId="0" applyFont="1"/></cellXfs><cellStyles count="1"><cellStyle name="Normal" xfId="0" builtinId="0"/></cellStyles></styleSheet>"#.to_string()),
        ];
        for (name, body) in parts {
            self.zip.start_file(name, o)?;
            self.zip.write_all(body.as_bytes())?;
        }
        let mut w = self.zip.finish()?;
        w.flush()?;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::Reader;
    use serde_json::json;

    #[test]
    fn writes_a_readable_workbook() {
        let dir = std::env::temp_dir().join(format!("xlsxstream_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.xlsx");
        let mut x = XlsxStream::create(path.to_str().unwrap(), "My: Sheet", vec!["Name".into(), "Amount".into(), "Ok".into()]).unwrap();
        x.write_row(&[json!("a <b> & \"c\"\u{1}\r\nline"), json!(12.5), json!(true)]).unwrap();
        x.write_row(&[json!(null), json!(3), json!("")]).unwrap();
        assert_eq!(x.finish().unwrap(), 1);
        let mut wb: calamine::Xlsx<_> = calamine::open_workbook(&path).unwrap();
        assert_eq!(wb.sheet_names(), vec!["My Sheet".to_string()]);
        let r = wb.worksheet_range("My Sheet").unwrap();
        assert_eq!(r.get_value((0, 1)).unwrap().to_string(), "Amount");
        assert_eq!(r.get_value((1, 0)).unwrap().to_string(), "a <b> & \"c\"\r\nline");
        assert_eq!(r.get_value((1, 1)).unwrap().to_string(), "12.5");
        assert_eq!(r.get_value((2, 1)).unwrap().to_string(), "3");
        assert_eq!(col_name(0), "A");
        assert_eq!(col_name(25), "Z");
        assert_eq!(col_name(26), "AA");
        assert_eq!(col_name(701), "ZZ");
        assert_eq!(col_name(702), "AAA");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
