//! Bulk data upload: read a spreadsheet and insert/update tracker records.

use crate::db::{self, i, s, vs, Row};
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, esc, now_str, Ctx};
use anyhow::{anyhow, Result};
use axum::response::Response;
use calamine::{Data, Reader};
use serde_json::{json, Value};

fn cell_value(c: &Data) -> Value {
    match c {
        Data::Empty => Value::Null,
        Data::String(x) => json!(x.trim()),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < 9e15 { json!(*f as i64) } else { json!(f) }
        }
        Data::Int(n) => json!(n),
        Data::Bool(b) => json!(b),
        Data::DateTime(d) => {
            // Excel serial date: days since 1899-12-30
            let f = d.as_f64();
            let secs = (f * 86400.0).round() as i64;
            match chrono::NaiveDate::from_ymd_opt(1899, 12, 30).and_then(|b| b.and_hms_opt(0, 0, 0)).map(|b| b + chrono::Duration::seconds(secs)) {
                Some(dt) => {
                    if dt.time() == chrono::NaiveTime::MIN { json!(dt.format("%Y-%m-%d").to_string()) } else { json!(dt.format("%Y-%m-%d %H:%M:%S").to_string()) }
                }
                None => json!(f),
            }
        }
        Data::DateTimeIso(x) | Data::DurationIso(x) => json!(x),
        Data::Error(_) => Value::Null,
    }
}

fn read_csv(path: &str) -> Result<Vec<Vec<Value>>> {
    let text = std::fs::read_to_string(path)?;
    let mut rows = vec![];
    for line in text.lines() {
        let mut cells = vec![];
        let mut cur = String::new();
        let mut quoted = false;
        let mut chars = line.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '"' if quoted && chars.peek() == Some(&'"') => {
                    cur.push('"');
                    chars.next();
                }
                '"' => quoted = !quoted,
                ',' if !quoted => {
                    cells.push(json!(cur.trim()));
                    cur.clear();
                }
                c => cur.push(c),
            }
        }
        cells.push(json!(cur.trim()));
        rows.push(cells);
    }
    Ok(rows)
}

/// Read one sheet as rows of JSON values (dates become ISO strings).
pub fn read_sheet(path: &str, sheet: usize) -> Result<Vec<Vec<Value>>> {
    if path.to_lowercase().ends_with(".csv") {
        return read_csv(path);
    }
    let mut wb = calamine::open_workbook_auto(path).map_err(|e| anyhow!("Cannot open {}: {}", path, e))?;
    let names = wb.sheet_names().to_vec();
    let name = names.get(sheet).ok_or_else(|| anyhow!("Sheet {} not found", sheet))?;
    let range = wb.worksheet_range(name).map_err(|e| anyhow!("{}", e))?;
    let (sr, sc) = range.start().unwrap_or((0, 0));
    let mut out: Vec<Vec<Value>> = vec![vec![]; sr as usize];
    for r in range.rows() {
        let mut line: Vec<Value> = vec![Value::Null; sc as usize];
        line.extend(r.iter().map(cell_value));
        out.push(line);
    }
    Ok(out)
}

/// Read every sheet of an uploaded spreadsheet (.xlsx/.xls/.ods/.csv/.txt) as trimmed
/// text rows, dropping trailing empty cells and rows.
pub fn read_all_sheets(path: &str, filename: &str, max_sheets: usize, max_rows: usize, max_cols: usize) -> Result<(Vec<(String, Vec<Vec<String>>)>, bool)> {
    let lower = filename.to_lowercase();
    let mut clipped = false;
    let tidy = |rows: Vec<Vec<String>>, clipped: &mut bool| -> Vec<Vec<String>> {
        let mut out: Vec<Vec<String>> = rows
            .into_iter()
            .map(|r| {
                let mut c: Vec<String> = r
                    .into_iter()
                    .map(|x| x.replace(['\r', '\n', '\t'], " ").trim().to_string())
                    .collect();
                while c.last().map(|s| s.is_empty()).unwrap_or(false) {
                    c.pop();
                }
                if c.len() > max_cols {
                    c.truncate(max_cols);
                    *clipped = true;
                }
                c
            })
            .collect();
        while out.last().map(|r| r.is_empty()).unwrap_or(false) {
            out.pop();
        }
        out
    };
    if lower.ends_with(".csv") || lower.ends_with(".txt") {
        let bytes = std::fs::read(path)?;
        let text = match String::from_utf8(bytes.clone()) {
            Ok(s) => s,
            Err(_) => bytes.into_iter().map(|b| b as char).collect(),
        };
        let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
        let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        let sep = [',', ';', '\t'].into_iter().max_by_key(|&c| first.matches(c).count()).unwrap_or(',');
        let mut rows: Vec<Vec<String>> = vec![];
        let mut row: Vec<String> = vec![];
        let mut cell = String::new();
        let mut quoted = false;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if quoted {
                if ch == '"' {
                    if chars.peek() == Some(&'"') {
                        cell.push('"');
                        chars.next();
                    } else {
                        quoted = false;
                    }
                } else {
                    cell.push(ch);
                }
            } else if ch == '"' && cell.is_empty() {
                quoted = true;
            } else if ch == sep {
                row.push(std::mem::take(&mut cell));
            } else if ch == '\n' || ch == '\r' {
                if ch == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut cell));
                rows.push(std::mem::take(&mut row));
                if rows.len() >= max_rows {
                    clipped = true;
                    break;
                }
            } else {
                cell.push(ch);
            }
        }
        if !cell.is_empty() || !row.is_empty() {
            row.push(cell);
            rows.push(row);
        }
        let sname = filename.rsplit_once('.').map(|(a, _)| a).unwrap_or(filename).to_string();
        let cleaned = tidy(rows, &mut clipped);
        let out = if cleaned.is_empty() { vec![] } else { vec![(sname, cleaned)] };
        return Ok((out, clipped));
    }
    let mut wb = calamine::open_workbook_auto(path).map_err(|e| anyhow!("Cannot open {}: {}", filename, e))?;
    let names = wb.sheet_names().to_vec();
    if names.len() > max_sheets {
        clipped = true;
    }
    let mut sheets = vec![];
    for name in names.into_iter().take(max_sheets) {
        let Ok(range) = wb.worksheet_range(&name) else { continue };
        let (sr, sc) = range.start().unwrap_or((0, 0));
        let mut rows: Vec<Vec<String>> = vec![vec![]; (sr as usize).min(max_rows)];
        for r in range.rows() {
            if rows.len() >= max_rows {
                clipped = true;
                break;
            }
            let mut line: Vec<String> = vec![String::new(); (sc as usize).min(max_cols + 1)];
            for c in r.iter().take(max_cols + 1 - line.len()) {
                line.push(vs(&cell_value(c)));
            }
            rows.push(line);
        }
        let cleaned = tidy(rows, &mut clipped);
        if !cleaned.is_empty() {
            sheets.push((name, cleaned));
        }
    }
    Ok((sheets, clipped))
}

fn norm(x: &str) -> String {
    x.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect()
}

/// Read header columns from `header_start..=header_end` (1-based) for the column-mapping wizard.
/// Returns `(col_index, normalized_name, display_text)` for each non-empty header column.
pub fn extract_headers(path: &str, header_start: usize, header_end: usize) -> Result<Vec<(usize, String, String)>> {
    let rows = read_sheet(path, 0)?;
    let hs = header_start.max(1);
    let he = header_end.max(hs);
    let heads: Vec<&Vec<Value>> = rows.iter().enumerate().filter(|(idx, _)| { let rn = idx + 1; rn >= hs && rn <= he }).map(|(_, r)| r).collect();
    let width = heads.iter().map(|r| r.len()).max().unwrap_or(0);
    let mut out = vec![];
    for c in 0..width {
        let parts: Vec<String> = heads.iter().filter_map(|r| r.get(c)).map(vs).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect();
        if parts.is_empty() {
            continue;
        }
        let text = parts.join(" ");
        let mut slug = text.to_lowercase().chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '_' }).collect::<String>();
        while slug.contains("__") {
            slug = slug.replace("__", "_");
        }
        let slug = slug.trim_matches('_').to_string();
        out.push((c, slug, text));
    }
    Ok(out)
}

/// Stream the rows of the first sheet into `tx` as (1-based row number, cells).
/// .xlsx/.xlsm files are read cell by cell (never the whole sheet in memory); other
/// formats (.xls, .ods, .csv) are read whole and then sent.
fn stream_sheet(path: &str, tx: tokio::sync::mpsc::Sender<(usize, Vec<Value>)>) -> Result<()> {
    let lower = path.to_lowercase();
    if lower.ends_with(".xlsx") || lower.ends_with(".xlsm") {
        let mut wb: calamine::Xlsx<_> = calamine::open_workbook(path).map_err(|e| anyhow!("Cannot open {}: {}", path, e))?;
        let name = wb.sheet_names().first().cloned().ok_or_else(|| anyhow!("The workbook has no sheets"))?;
        let mut cells = wb.worksheet_cells_reader(&name).map_err(|e| anyhow!("{}", e))?;
        let mut cur: Option<u32> = None;
        let mut line: Vec<Value> = vec![];
        while let Some(cell) = cells.next_cell().map_err(|e| anyhow!("{}", e))? {
            let (r, c) = cell.get_position();
            if cur != Some(r) {
                if let Some(pr) = cur {
                    if tx.blocking_send((pr as usize + 1, std::mem::take(&mut line))).is_err() {
                        return Ok(()); // the importer stopped early
                    }
                }
                cur = Some(r);
            }
            let v = cell_ref_value(cell.get_value());
            let c = c as usize;
            if line.len() <= c {
                line.resize(c + 1, Value::Null);
            }
            line[c] = v;
        }
        if let Some(pr) = cur {
            let _ = tx.blocking_send((pr as usize + 1, line));
        }
        return Ok(());
    }
    for (n, line) in read_sheet(path, 0)?.into_iter().enumerate() {
        if tx.blocking_send((n + 1, line)).is_err() {
            break;
        }
    }
    Ok(())
}

fn cell_ref_value(c: &calamine::DataRef) -> Value {
    use calamine::DataRef as D;
    match c {
        D::Empty => Value::Null,
        D::String(x) => json!(x.trim()),
        D::SharedString(x) => json!(x.trim()),
        D::Float(f) => cell_value(&Data::Float(*f)),
        D::Int(n) => json!(n),
        D::Bool(b) => json!(b),
        D::DateTime(d) => cell_value(&Data::DateTime(*d)),
        D::DateTimeIso(x) | D::DurationIso(x) => json!(x),
        D::Error(_) => Value::Null,
    }
}

const BATCH_ROWS: usize = 500;

/// Columns of the data table an upload writes, in order, with the field that
/// coerces each value (None = stored as text, e.g. record_status of a Tracker).
struct Target {
    cols: Vec<(String, Option<Row>)>,
}

impl Target {
    fn values_sql(&self, rec: &Row, data_id: i64) -> String {
        let mut v: Vec<String> = self
            .cols
            .iter()
            .map(|(name, f)| match (rec.get(name), f) {
                (None, _) => "NULL".to_string(),
                (Some(x), Some(f)) => db::lit(&crate::tracker::coerce(f, x)),
                (Some(x), None) => db::lit(&json!(vs(x))),
            })
            .collect();
        v.push(data_id.to_string());
        format!("({})", v.join(","))
    }
}

/// Insert a batch with one statement. If the database rejects it (one bad value
/// fails the whole statement), insert the rows one by one so only the bad rows are
/// rejected and each gets its own error message.
async fn insert_batch(db: &db::Db, t: &Tracker, target: &Target, batch: &mut Vec<(usize, Row)>, data_id: i64, errs: &mut Vec<String>) -> (usize, usize) {
    if batch.is_empty() {
        return (0, 0);
    }
    let Ok(dt) = db::ident(&t.data_table()) else { return (0, batch.len()) };
    let cols = target.cols.iter().map(|(n, _)| db::qcol(n)).chain(std::iter::once("dataupdate_id".to_string())).collect::<Vec<_>>().join(",");
    let head = format!("insert into {} ({}) values ", dt, cols);
    let all = batch.iter().map(|(_, r)| target.values_sql(r, data_id)).collect::<Vec<_>>().join(",");
    let (mut ok, mut bad) = (0, 0);
    if db.exec(&format!("{}{}", head, all), &db::no_params()).await.is_ok() {
        ok = batch.len();
    } else {
        for (rn, r) in batch.iter() {
            match db.exec(&format!("{}{}", head, target.values_sql(r, data_id)), &db::no_params()).await {
                Ok(_) => ok += 1,
                Err(e) => {
                    bad += 1;
                    if errs.len() < 20 {
                        errs.push(format!("Row {}: {}", rn, e.to_string().chars().take(200).collect::<String>()));
                    }
                }
            }
        }
    }
    batch.clear();
    (ok, bad)
}

/// Process a portal_tracker_data row: map header columns to fields (either from
/// explicit g6ikram `datasource_{field_id}`/`custom_{field_id}`/`update_{field_id}`
/// savedparams, or automatically by header name/label) and write each data row.
pub async fn process(st: &web::St, data_id: i64) -> Result<String> {
    let db = &st.db;
    let d = db.get("portal_tracker_data", data_id).await?.ok_or_else(|| anyhow!("Upload not found"))?;
    let t = Tracker::by_id(db, i(&d, "tracker_id").unwrap_or(0)).await.ok_or_else(|| anyhow!("Tracker not found"))?;
    let _ = db.update("portal_tracker_data", data_id, &row! {"upload_status"=>-1i64}).await;
    let path = match i(&d, "file_link_id") {
        Some(f) => db.get("file_link", f).await?.map(|f| s(&f, "path")).unwrap_or_else(|| s(&d, "path")),
        None => s(&d, "path"),
    };
    if path.is_empty() || !std::path::Path::new(&path).exists() {
        return Err(anyhow!("Uploaded file not found on disk"));
    }
    let opts: Value = serde_json::from_str(&s(&d, "savedparams")).unwrap_or(Value::Null);
    let opts_obj = opts.as_object();
    let ignore_id = opts.get("id_column").and_then(|v| v.as_str()) == Some("ignore");
    let has_explicit_map = opts_obj.map(|o| o.keys().any(|k| k.starts_with("datasource_"))).unwrap_or(false);

    let (tx, mut rx) = tokio::sync::mpsc::channel::<(usize, Vec<Value>)>(2000);
    let path2 = path.clone();
    let reader = tokio::task::spawn_blocking(move || stream_sheet(&path2, tx));
    let hs = i(&d, "header_start").unwrap_or(1).max(1) as usize;
    let he = i(&d, "header_end").unwrap_or(hs as i64).max(hs as i64) as usize;
    let ds = i(&d, "data_row").unwrap_or(he as i64 + 1).max(1) as usize;
    let de = i(&d, "data_end").filter(|x| *x > 0).map(|x| x as usize).unwrap_or(usize::MAX);

    // header rows
    let mut heads: Vec<Vec<Value>> = vec![];
    let mut pending: Option<(usize, Vec<Value>)> = None;
    while let Some((rn, line)) = rx.recv().await {
        if rn < hs {
            continue;
        }
        if rn > he {
            pending = Some((rn, line));
            break;
        }
        heads.push(line);
    }

    let width = heads.iter().map(|r| r.len()).max().unwrap_or(0);
    // For each field: either mapped from a column index or a constant custom value
    let mut col_bindings: Vec<(usize, String)> = vec![];
    let mut custom_vals: Vec<(String, Value)> = vec![];
    let mut key_fields: Vec<String> = vec![];
    let mut mapped: Vec<String> = vec![];
    let rs_is_field = t.tracker_type == "DataStore" && t.field("record_status").is_some();

    if has_explicit_map {
        let obj = opts_obj.unwrap();
        for f in &t.fields {
            let fid = i(f, "id").unwrap_or(0);
            let fname = s(f, "name");
            let ds_key = format!("datasource_{fid}");
            let ds_val = obj.get(&ds_key).map(vs).unwrap_or_else(|| "ignore".into());
            if ds_val.is_empty() || ds_val == "ignore" {
                continue;
            }
            if ds_val == "custom" {
                let cv = obj.get(&format!("custom_{fid}")).map(vs).unwrap_or_default();
                custom_vals.push((fname.clone(), json!(cv)));
                if !mapped.contains(&fname) {
                    mapped.push(fname.clone());
                }
            } else if let Ok(cidx) = ds_val.parse::<usize>() {
                col_bindings.push((cidx, fname.clone()));
                if !mapped.contains(&fname) {
                    mapped.push(fname.clone());
                }
            }
            let upd_val = obj.get(&format!("update_{fid}")).map(vs).unwrap_or_default();
            if !upd_val.is_empty() && upd_val != "false" && upd_val != "0" {
                key_fields.push(fname);
            }
        }
    } else {
        for c in 0..width {
            let head: Vec<String> = heads.iter().filter_map(|x| x.get(c)).map(vs).filter(|x| !x.is_empty()).collect();
            let h = norm(&head.join(" "));
            let last = head.last().map(|x| norm(x)).unwrap_or_default();
            if h.is_empty() {
                continue;
            }
            if h == "id" {
                if !ignore_id {
                    col_bindings.push((c, "id".into()));
                    if !mapped.contains(&"id".to_string()) {
                        mapped.push("id".into());
                    }
                }
            } else if h == "recordstatus" || h == "status" && t.field("status").is_none() {
                col_bindings.push((c, "record_status".into()));
                if !mapped.contains(&"record_status".to_string()) {
                    mapped.push("record_status".into());
                }
            } else if let Some(f) = t.fields.iter().find(|f| norm(&s(f, "name")) == h || norm(&s(f, "label")) == h || norm(&s(f, "name")) == last || norm(&s(f, "label")) == last) {
                let fname = s(f, "name");
                col_bindings.push((c, fname.clone()));
                if !mapped.contains(&fname) {
                    mapped.push(fname);
                }
            }
        }
    }

    if mapped.is_empty() {
        drop(rx);
        let _ = reader.await;
        return Err(anyhow!("None of the file's column headers matched a field of {}", t.name));
    }
    let initial = t.status_name(t.initial_status());
    let add_initial = t.tracker_type != "DataStore" && !initial.is_empty();
    let mut cols: Vec<(String, Option<Row>)> = vec![];
    for m in &mapped {
        if m == "id" {
            continue;
        }
        if m == "record_status" && !rs_is_field {
            if t.tracker_type != "DataStore" {
                cols.push((m.clone(), None));
            }
        } else if let Some(f) = t.field(m) {
            if crate::tracker::sql_type(&s(f, "field_type")).is_some() {
                cols.push((m.clone(), Some(f.clone())));
            }
        }
    }
    if add_initial && !cols.iter().any(|(n, _)| n == "record_status") {
        cols.push(("record_status".into(), None));
    }
    let target = Target { cols };
    let validators: Vec<(String, regex::Regex)> = db::csv(&s(&t.row, "row_validation_fields"))
        .into_iter()
        .filter_map(|n| t.field(&n).map(|f| (n.clone(), s(f, "row_validation_regex"))))
        .filter(|(_, r)| !r.is_empty())
        .filter_map(|(n, r)| regex::Regex::new(&format!("^(?:{})$", r)).ok().map(|re| (n, re)))
        .collect();
    let (mut ins, mut upd, mut skip, mut failed, mut errs) = (0usize, 0usize, 0usize, 0usize, vec![]);
    let mut batch: Vec<(usize, Row)> = Vec::with_capacity(BATCH_ROWS);
    let mut seen = 0usize;
    let dt_ident = db::ident(&t.data_table())?;
    loop {
        let next = match pending.take() {
            Some(x) => Some(x),
            None => rx.recv().await,
        };
        let Some((rn, line)) = next else { break };
        if rn < ds {
            continue;
        }
        if rn > de {
            break;
        }
        let mut rec = Row::new();
        let mut has_excel_cell = false;
        for (c, name) in &col_bindings {
            if let Some(v) = line.get(*c) {
                if !v.is_null() && !vs(v).is_empty() {
                    rec.insert(name.clone(), v.clone());
                    has_excel_cell = true;
                }
            }
        }
        if !has_excel_cell && !col_bindings.is_empty() {
            skip += 1;
            continue;
        }
        for (name, v) in &custom_vals {
            if !rec.contains_key(name) && !vs(v).is_empty() {
                rec.insert(name.clone(), v.clone());
            }
        }
        if rec.is_empty() || !validators.iter().all(|(n, re)| re.is_match(&s(&rec, n))) {
            skip += 1;
            continue;
        }
        // Key Update upsert (g6ikram `update_{field_id}` checkboxes)
        if !key_fields.is_empty() && i(&rec, "id").is_none() {
            let mut kcond = vec![];
            let mut all_present = true;
            for kf in &key_fields {
                if let Some(val) = rec.get(kf) {
                    let coerced = match t.field(kf) {
                        Some(f) => crate::tracker::coerce(f, val),
                        None => val.clone(),
                    };
                    if coerced.is_null() {
                        all_present = false;
                    } else {
                        kcond.push(format!("{}={}", db::qcol(kf), db::lit(&coerced)));
                    }
                } else {
                    all_present = false;
                }
            }
            if all_present && !kcond.is_empty() {
                if let Ok(Some(found)) = db.first(&format!("select id from {} where {} order by id desc limit 1", dt_ident, kcond.join(" and ")), &db::no_params()).await {
                    if let Some(existing_id) = i(&found, "id") {
                        rec.insert("id".into(), json!(existing_id));
                    }
                }
            }
        }
        if i(&rec, "id").is_some() {
            rec.insert("dataupdate_id".into(), json!(data_id));
            match t.save(db, &rec).await {
                Ok(_) => upd += 1,
                Err(e) => {
                    failed += 1;
                    if errs.len() < 20 {
                        errs.push(format!("Row {}: {}", rn, e.to_string().chars().take(200).collect::<String>()));
                    }
                }
            }
            continue;
        }
        if add_initial && !rec.contains_key("record_status") {
            rec.insert("record_status".into(), json!(initial));
        }
        batch.push((rn, rec));
        if batch.len() >= BATCH_ROWS {
            let (ok, bad) = insert_batch(db, &t, &target, &mut batch, data_id, &mut errs).await;
            ins += ok;
            failed += bad;
        }
        seen += 1;
        if seen % 20000 == 0 {
            let _ = db.update("portal_tracker_data", data_id, &row! {"messages"=>format!("Processing: {} rows read so far", seen)}).await;
        }
    }
    let (ok, bad) = insert_batch(db, &t, &target, &mut batch, data_id, &mut errs).await;
    ins += ok;
    failed += bad;
    drop(rx);
    reader.await.map_err(|e| anyhow!("{}", e))??;
    let total_recv = ins + upd + skip + failed;
    let uploaded_cnt = ins + upd;
    let fname = std::path::Path::new(&path).file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_else(|| "Unknown".into());
    let mut msg = format!(
        "File: {fname}\nStatus: Completed\nRecords Received: {total_recv}\nRecords Uploaded: {uploaded_cnt}"
    );
    if upd > 0 {
        msg.push_str(&format!("\nRecords Inserted: {ins}\nRecords Updated: {upd}"));
    }
    if skip > 0 {
        msg.push_str(&format!("\nRecords Skipped (non-data rows): {skip}"));
    }
    msg.push_str(&format!("\nRecords Rejected: {failed}"));
    if !errs.is_empty() {
        msg.push_str("\n\n=== Rejected Row Details ===\n");
        msg.push_str(&errs.join("\n"));
    }
    db.update("portal_tracker_data", data_id, &row! {"uploaded"=>true,"upload_status"=>if failed == 0 { 1i64 } else { 2i64 },"messages"=>msg.clone()}).await?;
    Ok(msg)
}

/// Run `process` in the background (large files take minutes) and record it in
/// portal_background_trail like the Grails portal's PortalBackgroundTrail.
pub fn spawn_process(st: web::St, data_id: i64, user_id: Option<i64>) {
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let start = now_str();
        let res = process(&st, data_id).await;
        if let Err(e) = &res {
            let _ = st.db.update("portal_tracker_data", data_id, &row! {"messages"=>format!("Upload failed: {}", e),"upload_status"=>3}).await;
        }
        let _ = st
            .db
            .insert(
                "portal_background_trail",
                &row! {"name"=>"portalTrackerData.upload","details"=>format!("PortalTrackerData {}", data_id),"user_id"=>user_id,"start_date"=>start,
                "duration_ms"=>started.elapsed().as_millis() as i64,"success"=>res.is_ok(),"error"=>res.err().map(|e| e.to_string().chars().take(1000).collect::<String>())},
            )
            .await;
    });
}

/// GET shows the upload form, POST stores the file and processes it.
pub async fn upload(mut ctx: Ctx) -> Response {
    let tid = ctx.pi("tracker_id").unwrap_or(0);
    let Some(t) = Tracker::by_id(ctx.db(), tid).await else {
        ctx.flash("Choose a tracker first");
        return ctx.redirect("/portalTracker/index").await;
    };
    if !ctx.can_admin(&t.module).await {
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "Only module administrators can upload data").await;
    }
    if ctx.method == "POST" {
        let Some(up) = ctx.files.remove("file") else {
            ctx.flash("No file was selected");
            return ctx.redirect(&format!("/portalTrackerData/upload?tracker_id={}", tid)).await;
        };
        let dir = format!("{}/dataupload/{}", ctx.st.cfg.upload_dir, t.slug);
        let slug = format!("dataupload_{}_{}", t.slug, chrono::Local::now().format("%Y%m%d%H%M%S"));
        let fid = match crate::files::store_upload(ctx.db(), &up, &dir, &t.module, &slug, Some(&t.slug), None).await {
            Ok(x) => x,
            Err(e) => {
                ctx.flash(&format!("Upload failed: {}", e));
                return ctx.redirect(&format!("/portalTrackerData/upload?tracker_id={}", tid)).await;
            }
        };
        let rec = row! {"tracker_id"=>tid,"module"=>t.module.clone(),"file_link_id"=>fid,"header_start"=>ctx.pi("header_start").unwrap_or(1),"header_end"=>ctx.pi("header_end").unwrap_or(1),
            "data_row"=>ctx.pi("data_row").unwrap_or(2),"data_end"=>ctx.pi("data_end"),"date_created"=>now_str(),"uploader_id"=>ctx.uid(),"uploaded"=>false,"upload_status"=>0,
            "messages"=>"Upload is currently in queue","savedparams"=>json!({"id_column": if ctx.p("id_column") == "ignore" { "ignore" } else { "update" }}).to_string()};
        let did = match ctx.db().insert("portal_tracker_data", &rec).await {
            Ok(x) => x,
            Err(e) => {
                ctx.flash(&e.to_string());
                return ctx.redirect("/portalTrackerData/index").await;
            }
        };
        spawn_process(ctx.st.clone(), did, ctx.uid());
        ctx.flash("The file was received and is being imported in the background; this page shows the result when it is done");
        return ctx.redirect(&format!("/portalTrackerData/show/{}", did)).await;
    }
    let labels: Vec<String> = t.fields.iter().filter(|f| crate::tracker::sql_type(&s(f, "field_type")).is_some()).map(|f| format!("<code>{}</code> ({})", esc(&s(f, "name")), esc(&s(f, "label")))).collect();
    let h = format!(
        "<div class='container'><h1>Upload data: {}</h1><p>The header row is matched against field names or labels. Include an <code>id</code> column to update existing records instead of adding new ones.</p><p>Fields: {}</p><form method='post' enctype='multipart/form-data' action='/portalTrackerData/upload?tracker_id={}'>{}<div class='mb-2'><label class='form-label'>Excel or CSV file</label><input type='file' class='form-control' name='file' required/></div><div class='row'><div class='col'><label class='form-label'>Header starts at row</label><input type='number' class='form-control' name='header_start' value='1'/></div><div class='col'><label class='form-label'>Header ends at row</label><input type='number' class='form-control' name='header_end' value='1'/></div><div class='col'><label class='form-label'>Data starts at row</label><input type='number' class='form-control' name='data_row' value='2'/></div><div class='col'><label class='form-label'>Data ends at row (blank = all)</label><input type='number' class='form-control' name='data_end'/></div></div><div class='mt-2'><label class='form-label'>A column headed <code>id</code></label><select class='form-select' name='id_column'><option value='update'>updates the record with that id</option><option value='ignore'>is ignored (every row is a new record)</option></select></div><button class='btn btn-primary mt-3'>Upload</button></form></div>",
        esc(&t.name), labels.join(", "), tid, ctx.csrf_field()
    );
    ctx.html("Upload data", h).await
}

/// Remove the records an upload inserted.
pub async fn rollback(mut ctx: Ctx, id: i64) -> Response {
    let Ok(Some(d)) = ctx.db().get("portal_tracker_data", id).await else { return ctx.redirect("/portalTrackerData/index").await };
    let Some(t) = Tracker::by_id(ctx.db(), i(&d, "tracker_id").unwrap_or(0)).await else { return ctx.redirect("/portalTrackerData/index").await };
    if ctx.method != "POST" || !ctx.can_admin(&t.module).await {
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let n = ctx.db().exec(&format!("delete from {} where dataupdate_id={}", db::qcol(&t.data_table()), id), &db::no_params()).await.unwrap_or(0);
    let _ = ctx.db().update("portal_tracker_data", id, &row! {"messages"=>format!("Rolled back: {} records removed", n),"uploaded"=>false}).await;
    ctx.flash(&format!("{} records removed", n));
    ctx.redirect(&format!("/portalTrackerData/show/{}", id)).await
}

// ---------------- module data dump (g6ikram PortalTrackerDataController.moduledump) ----------------

struct DumpJob {
    done: bool,
    file: Option<std::path::PathBuf>,
    error: Option<String>,
    filename: String,
}

static DUMP_JOBS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, DumpJob>>> = std::sync::OnceLock::new();

fn dump_jobs() -> &'static std::sync::Mutex<std::collections::HashMap<String, DumpJob>> {
    DUMP_JOBS.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn dump_waiting_page(token: &str, label: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
<meta charset="utf-8">
<title>Preparing data dump&hellip;</title>
<style>
body{{font-family:Arial,Helvetica,sans-serif;text-align:center;padding:60px 20px;background:#f5f5f5;color:#333;}}
.box{{background:#fff;border-radius:8px;padding:40px;display:inline-block;box-shadow:0 2px 8px rgba(0,0,0,.1);min-width:340px;}}
h2{{margin:0 0 6px;font-size:20px;}}
.sub{{color:#777;font-size:13px;margin:0 0 18px;}}
#status{{color:#666;margin:16px 0 0;}}
.spinner{{display:inline-block;width:40px;height:40px;border:4px solid #ddd;border-top-color:#0078d4;border-radius:50%;animation:spin .8s linear infinite;margin-bottom:16px;}}
@keyframes spin{{to{{transform:rotate(360deg);}}}}
.error{{color:#c00;}}
</style>
</head>
<body>
<div class="box">
  <div class="spinner" id="spinner"></div>
  <h2>Preparing data dump</h2>
  <p class="sub">{label}</p>
  <p id="status">Building the file on the server. A large tracker takes a minute or two &mdash; leave this tab open.</p>
</div>
<script>
var token = '{token}';
var baseUrl = window.location.href.split('?')[0];
var checkUrl = baseUrl + '?async_token=' + token + '&async_status=1';
var downloadUrl = baseUrl + '?async_token=' + token;
var waited = 0;
function check() {{
    fetch(checkUrl, {{credentials:'same-origin'}})
        .then(function(r){{ return r.json(); }})
        .then(function(data){{
            if(data.ready){{
                document.getElementById('spinner').style.display='none';
                document.getElementById('status').textContent='Ready \u2014 downloading\u2026';
                var a=document.createElement('a');
                a.href=downloadUrl; a.download='';
                document.body.appendChild(a); a.click(); document.body.removeChild(a);
                document.getElementById('status').textContent='Download started. You can close this tab.';
            }} else if(data.error){{
                document.getElementById('spinner').style.display='none';
                var st=document.getElementById('status'); st.textContent=data.error; st.className='error';
            }} else {{
                waited += 2;
                if(waited % 30 === 0){{
                    document.getElementById('status').textContent='Still building\u2026 ' + waited + 's so far.';
                }}
                setTimeout(check, 2000);
            }}
        }})
        .catch(function(){{ setTimeout(check, 2000); }});
}}
setTimeout(check, 2000);
</script>
</body>
</html>"#,
        token = esc(token),
        label = esc(label)
    )
}

async fn build_tracker_forimport_xlsx(db: &db::Db, t: &Tracker, out_path: &std::path::Path) -> Result<usize> {
    let mut ordered: Vec<&Row> = vec![];
    let preferred = if !s(&t.row, "excelfields").trim().is_empty() {
        db::csv(&s(&t.row, "excelfields"))
    } else {
        db::csv(&s(&t.row, "listfields"))
    };
    for name in &preferred {
        if let Some(f) = t.field(name) {
            if !ordered.iter().any(|x| s(x, "name") == s(f, "name")) {
                ordered.push(f);
            }
        }
    }
    for f in &t.fields {
        if !ordered.iter().any(|x| s(x, "name") == s(f, "name")) {
            ordered.push(f);
        }
    }
    // forimport=1: drop FieldGroup, HasMany, and computed field_query columns
    ordered.retain(|f| {
        let ft = s(f, "field_type");
        ft != "FieldGroup" && ft != "HasMany" && s(f, "field_query").trim().is_empty()
    });
    let mut header: Vec<String> = ordered.iter().map(|f| s(f, "name")).collect();
    let has_status = t.tracker_type != "DataStore" && !header.iter().any(|h| h == "record_status");
    if has_status {
        header.push("record_status".into());
    }
    let dt = db::ident(&t.data_table())?;
    let rows = db.rows(&format!("select * from {} order by id", dt), &db::no_params()).await?;
    let mut x = crate::xlsxstream::XlsxStream::create(out_path.to_str().unwrap_or(""), &t.name, header.clone())?;
    for r in &rows {
        let mut cells: Vec<Value> = Vec::with_capacity(header.len());
        for f in &ordered {
            let name = s(f, "name");
            let ft = s(f, "field_type");
            let v = r.get(&name).cloned().unwrap_or(Value::Null);
            let cell = if v.is_null() {
                Value::Null
            } else {
                match ft.as_str() {
                    "Date" => {
                        let txt = vs(&v);
                        match crate::script::parse_dt(&txt) {
                            Some(d) => json!(d.format("%Y-%m-%d").to_string()),
                            None => json!(txt),
                        }
                    }
                    "DateTime" => {
                        let txt = vs(&v);
                        match crate::script::parse_dt(&txt) {
                            Some(d) => json!(d.format("%Y-%m-%d %H:%M:%S").to_string()),
                            None => json!(txt),
                        }
                    }
                    "User" | "Branch" | "BelongsTo" | "File" => {
                        // Raw stored ID as text (never decimal float like "246.0")
                        if let Some(n) = v.as_i64() {
                            json!(n.to_string())
                        } else if let Some(fl) = v.as_f64() {
                            if fl.fract() == 0.0 { json!((fl as i64).to_string()) } else { json!(fl.to_string()) }
                        } else {
                            json!(vs(&v))
                        }
                    }
                    "Checkbox" => v,
                    _ => {
                        let txt = vs(&v);
                        if matches!(txt.chars().next(), Some('=' | '+' | '-' | '@')) {
                            json!(format!(" {}", txt))
                        } else {
                            v
                        }
                    }
                }
            };
            cells.push(cell);
        }
        if has_status {
            cells.push(r.get("record_status").cloned().unwrap_or(Value::Null));
        }
        x.write_row(&cells)?;
    }
    x.finish()?;
    Ok(rows.len())
}

/// GET /portalTrackerData/moduledump/{module_id} — zip of re-importable .xlsx files for every tracker in a module.
pub async fn moduledump(mut ctx: Ctx, module_id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    // Poll or download existing async dump job
    let tok = ctx.p("async_token");
    if !tok.is_empty() {
        if !ctx.p("async_status").is_empty() {
            let state = dump_jobs().lock().unwrap().get(&tok).map(|j| (j.done, j.file.is_some(), j.error.clone()));
            return match state {
                Some((true, true, None)) => ctx.json(json!({"ready": true})).await,
                Some((true, _, Some(err))) => ctx.json(json!({"ready": false, "error": err})).await,
                Some(_) => ctx.json(json!({"ready": false})).await,
                None => ctx.json(json!({"ready": false, "error": "Dump job expired or not found."})).await,
            };
        }
        let job = dump_jobs().lock().unwrap().remove(&tok);
        return match job {
            Some(DumpJob { file: Some(path), filename, .. }) => {
                let bytes = tokio::fs::read(&path).await.unwrap_or_default();
                let _ = tokio::fs::remove_file(&path).await;
                ctx.bytes("application/zip", Some(&filename), bytes, false).await
            }
            Some(DumpJob { error: Some(e), .. }) => ctx.status(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e).await,
            _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "Dump file not found").await,
        };
    }

    let Ok(Some(m)) = ctx.db().get("portal_module", module_id).await else {
        return ctx.status(axum::http::StatusCode::NOT_FOUND, "Module not found").await;
    };
    let mname = s(&m, "name");
    let ok = match &ctx.user {
        Some(u) => ctx.is_admin() || web::admin_list(ctx.db(), u).await.contains(&mname) || web::developer_list(ctx.db(), u).await.contains(&mname),
        None => false,
    };
    if !ok {
        ctx.flash(&format!("Only an admin or developer of {} can dump its data.", mname));
        return ctx.redirect(&format!("/portalModule/show/{}", module_id)).await;
    }

    let trackers = ctx
        .db()
        .rows("select id, slug, name from portal_tracker where module=:m order by slug", &row! {"m"=>mname.clone()})
        .await
        .unwrap_or_default();
    let token = uuid::Uuid::new_v4().to_string();
    let label = format!("{}_forimport_{}", mname, chrono::Local::now().format("%Y%m%d_%H%M"));
    let filename = format!("{}.zip", label);
    let count = trackers.len();
    dump_jobs().lock().unwrap().insert(
        token.clone(),
        DumpJob { done: false, file: None, error: None, filename: filename.clone() },
    );

    let st = ctx.st.clone();
    let uid = ctx.uid();
    let userid = ctx.user.as_ref().map(|u| s(u, "userid")).unwrap_or_default();
    let mname2 = mname.clone();
    let tok2 = token.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let start = now_str();
        let dir = std::path::PathBuf::from(&st.cfg.upload_dir).join("tmp");
        let _ = std::fs::create_dir_all(&dir);
        let zip_path = dir.join(format!("moduledump_{}.zip", tok2));
        let res: Result<()> = async {
            let mut report = vec![
                format!("Data dump (for re-import) of module {}", mname2),
                format!("Built {} for {}", now_str(), userid),
                String::new(),
            ];
            let mut failed = 0usize;
            let zf = std::fs::File::create(&zip_path)?;
            let mut zw = zip::ZipWriter::new(zf);
            let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
            for tr in &trackers {
                let tid = i(tr, "id").unwrap_or(0);
                let tslug = s(tr, "slug");
                let tname = s(tr, "name");
                let tmp_xlsx = dir.join(format!("dump_{}_{}.xlsx", tok2, tid));
                let entry_name = format!("{}_forimport.xlsx", tslug);
                match Tracker::by_id(&st.db, tid).await {
                    Some(tk) => match build_tracker_forimport_xlsx(&st.db, &tk, &tmp_xlsx).await {
                        Ok(nrows) => {
                            let data = std::fs::read(&tmp_xlsx)?;
                            use std::io::Write;
                            zw.start_file(&entry_name, opts)?;
                            zw.write_all(&data)?;
                            report.push(format!("OK      {}  ({} - {} rows)", entry_name, tname, nrows));
                        }
                        Err(e) => {
                            failed += 1;
                            report.push(format!("FAILED  {}  ({}): {}", tslug, tname, e));
                            web::log_error(&st.db, "portalTrackerData", "moduledump", &e.to_string(), Some(&mname2), Some(&tslug), uid).await;
                        }
                    },
                    None => {
                        failed += 1;
                        report.push(format!("FAILED  {}  ({}): tracker not found", tslug, tname));
                    }
                }
                let _ = std::fs::remove_file(&tmp_xlsx);
            }
            report.push(String::new());
            report.push(format!(
                "{} of {} tracker(s) dumped{}.",
                trackers.len().saturating_sub(failed),
                trackers.len(),
                if failed > 0 { format!(", {} failed", failed) } else { String::new() }
            ));
            use std::io::Write;
            zw.start_file("dump_report.txt", opts)?;
            zw.write_all(report.join("\r\n").as_bytes())?;
            zw.finish()?;
            Ok(())
        }
        .await;

        let _ = st
            .db
            .insert(
                "portal_background_trail",
                &row! {
                    "name"=>"portalTrackerData.moduledump",
                    "details"=>format!("module {} ({} trackers)", mname2, count),
                    "user_id"=>uid,
                    "start_date"=>start,
                    "duration_ms"=>started.elapsed().as_millis() as i64,
                    "success"=>res.is_ok(),
                    "error"=>res.as_ref().err().map(|e| e.to_string().chars().take(1000).collect::<String>())
                },
            )
            .await;

        let mut map = dump_jobs().lock().unwrap();
        if let Some(j) = map.get_mut(&tok2) {
            j.done = true;
            match res {
                Ok(()) => j.file = Some(zip_path),
                Err(e) => j.error = Some(format!("Dump failed: {}", e)),
            }
        }
    });

    ctx.text("text/html; charset=utf-8", dump_waiting_page(&token, &format!("{} - all {} tracker(s)", mname, count))).await
}

/// GET /portalTrackerData/datadump/{tracker_id}[?forimport=1] — async single-tracker .xlsx dump.
pub async fn datadump(ctx: Ctx, tracker_id: i64) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let tok = ctx.p("async_token");
    if !tok.is_empty() {
        if !ctx.p("async_status").is_empty() {
            let state = dump_jobs().lock().unwrap().get(&tok).map(|j| (j.done, j.file.is_some(), j.error.clone()));
            return match state {
                Some((true, true, None)) => ctx.json(json!({"ready": true})).await,
                Some((true, _, Some(err))) => ctx.json(json!({"ready": false, "error": err})).await,
                Some(_) => ctx.json(json!({"ready": false})).await,
                None => ctx.json(json!({"ready": false, "error": "Job not found or expired"})).await,
            };
        }
        let job = dump_jobs().lock().unwrap().remove(&tok);
        return match job {
            Some(DumpJob { file: Some(path), filename, .. }) => {
                let bytes = tokio::fs::read(&path).await.unwrap_or_default();
                let _ = tokio::fs::remove_file(&path).await;
                ctx.bytes("application/vnd.openxmlformats-officedocument.spreadsheetml.sheet", Some(&filename), bytes, false).await
            }
            Some(DumpJob { error: Some(e), .. }) => ctx.status(axum::http::StatusCode::INTERNAL_SERVER_ERROR, &e).await,
            _ => ctx.status(axum::http::StatusCode::NOT_FOUND, "Dump not ready or expired").await,
        };
    }

    let Some(t) = Tracker::by_id(ctx.db(), tracker_id).await else {
        return ctx.status(axum::http::StatusCode::NOT_FOUND, "Tracker not found").await;
    };
    if !ctx.can_admin(&t.module).await {
        return ctx.status(axum::http::StatusCode::FORBIDDEN, "Not allowed").await;
    }

    let forimport = ctx.p("forimport") == "1";
    let label = format!("{}{}", t.slug, if forimport { "_forimport" } else { "" });
    let filename = format!("{label}.xlsx");
    let token = uuid::Uuid::new_v4().to_string();
    dump_jobs().lock().unwrap().insert(
        token.clone(),
        DumpJob { done: false, file: None, error: None, filename },
    );

    let st = ctx.st.clone();
    let uid = ctx.uid();
    let tok2 = token.clone();
    let tname = t.name.clone();
    let label2 = label.clone();
    tokio::spawn(async move {
        let started = std::time::Instant::now();
        let start = now_str();
        let dir = std::path::PathBuf::from(&st.cfg.upload_dir).join("tmp");
        let _ = std::fs::create_dir_all(&dir);
        let out_path = dir.join(format!("datadump_{}.xlsx", tok2));
        let res = build_tracker_forimport_xlsx(&st.db, &t, &out_path).await;
        let _ = st
            .db
            .insert(
                "portal_background_trail",
                &row! {
                    "name"=>"portalTrackerData.datadump",
                    "details"=>format!("tracker {} {}", tracker_id, label2),
                    "user_id"=>uid,
                    "start_date"=>start,
                    "duration_ms"=>started.elapsed().as_millis() as i64,
                    "success"=>res.is_ok(),
                    "error"=>res.as_ref().err().map(|e| e.to_string().chars().take(1000).collect::<String>())
                },
            )
            .await;
        let mut map = dump_jobs().lock().unwrap();
        if let Some(j) = map.get_mut(&tok2) {
            j.done = true;
            match res {
                Ok(_) => j.file = Some(out_path),
                Err(e) => j.error = Some(format!("Dump failed: {}", e)),
            }
        }
    });

    ctx.text("text/html; charset=utf-8", dump_waiting_page(&token, &tname)).await
}
