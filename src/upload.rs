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

/// Process a portal_tracker_data row: map header columns to fields (by name or
/// label) and write each data row. The sheet is streamed and new records are
/// inserted BATCH_ROWS at a time, so memory stays flat whatever the file size.
/// A column mapped to `id` updates the matching record instead of inserting,
/// unless the upload was made with "id column: ignore".
pub async fn process(st: &web::St, data_id: i64) -> Result<String> {
    let db = &st.db;
    let d = db.get("portal_tracker_data", data_id).await?.ok_or_else(|| anyhow!("Upload not found"))?;
    let t = Tracker::by_id(db, i(&d, "tracker_id").unwrap_or(0)).await.ok_or_else(|| anyhow!("Tracker not found"))?;
    let path = match i(&d, "file_link_id") {
        Some(f) => db.get("file_link", f).await?.map(|f| s(&f, "path")).unwrap_or_default(),
        None => s(&d, "path"),
    };
    let opts: Value = serde_json::from_str(&s(&d, "savedparams")).unwrap_or(Value::Null);
    let ignore_id = opts.get("id_column").and_then(|v| v.as_str()) == Some("ignore");
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
    let mut map: Vec<Option<String>> = vec![None; width];
    let rs_is_field = t.tracker_type == "DataStore" && t.field("record_status").is_some();
    for c in 0..width {
        let head: Vec<String> = heads.iter().filter_map(|x| x.get(c)).map(vs).filter(|x| !x.is_empty()).collect();
        let h = norm(&head.join(" "));
        let last = head.last().map(|x| norm(x)).unwrap_or_default();
        if h.is_empty() {
            continue;
        }
        if h == "id" {
            if !ignore_id {
                map[c] = Some("id".into());
            }
        } else if h == "recordstatus" || h == "status" && t.field("status").is_none() {
            map[c] = Some("record_status".into());
        } else if let Some(f) = t.fields.iter().find(|f| norm(&s(f, "name")) == h || norm(&s(f, "label")) == h || norm(&s(f, "name")) == last || norm(&s(f, "label")) == last) {
            map[c] = Some(s(f, "name"));
        }
    }
    let mut mapped: Vec<String> = vec![];
    for m in map.iter().flatten() {
        if !mapped.contains(m) {
            mapped.push(m.clone());
        }
    }
    if mapped.is_empty() {
        drop(rx);
        let _ = reader.await;
        return Err(anyhow!("None of the header columns matched a field of {}", t.name));
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
        for (c, m) in map.iter().enumerate() {
            if let (Some(name), Some(v)) = (m, line.get(c)) {
                if !v.is_null() && !vs(v).is_empty() {
                    rec.insert(name.clone(), v.clone());
                }
            }
        }
        if rec.is_empty() || !validators.iter().all(|(n, re)| re.is_match(&s(&rec, n))) {
            skip += 1;
            continue;
        }
        if i(&rec, "id").is_some() {
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
    let mut msg = format!("{} inserted, {} updated, {} skipped, {} rejected. Columns used: {}.", ins, upd, skip, failed, mapped.join(", "));
    if !errs.is_empty() {
        msg.push_str(&format!(" Errors: {}", errs.join(" | ")));
    }
    db.update("portal_tracker_data", data_id, &row! {"uploaded"=>true,"upload_status"=>if errs.is_empty() { 1 } else { 2 },"messages"=>msg.clone()}).await?;
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
