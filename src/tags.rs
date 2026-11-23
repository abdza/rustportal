//! Tracker "tags": HTML generators for the filter bar, list, form, record
//! display, transition buttons and audit trail. Used by the default tracker
//! screens and callable from page templates.

use crate::db::{self, b, csv, has, i, qcol, s, vs, Row};
use crate::row;
use crate::script::{fmt_number, option_pairs, Sc};
use crate::tracker::{other_format, Tracker, Trans};
use crate::web::{self, enc, esc};
use serde_json::{json, Value};

/// Home link on a tracker's own pages: the module's `home_page` setting, else the list.
pub async fn home_url(sc: &Sc, t: &Tracker) -> String {
    web::setting_str(sc.db(), &format!("{}.home_page", t.module)).await.unwrap_or_else(|| t.url("list"))
}

/// The Grails portal's navigation row (Home / New ... / Excel).
pub async fn tracker_nav(sc: &Sc, t: &Tracker, list: bool) -> String {
    let mut o = format!("<div class=\"nav\" role=\"navigation\"><ul><li><a class=\"home\" href=\"{}\">Home</a></li>", esc(&home_url(sc, t).await));
    if let Some(nt) = t.new_transition() {
        if t.transition_allowed(sc, &nt.name, None).await && (sc.inner.user.is_some() || nt.roles.is_empty()) {
            o.push_str(&format!("<li><a href=\"{}\" class=\"create\">New {}</a></li>", t.url("create"), esc(&t.name)));
        }
    }
    if list && b(&t.row, "downloadexcel") {
        let q = web::qs(&sc.inner.params, &["max", "offset", "excel"]);
        o.push_str(&format!("<li><a href=\"{}?excel=1{}\" class=\"list\">Excel</a></li>", t.url("list"), if q.is_empty() { String::new() } else { format!("&amp;{}", esc(&q)) }));
    }
    o.push_str("</ul></div>");
    o
}

async fn user_name(sc: &Sc, id: i64) -> String {
    let key = format!("uname_{}", id);
    if let Some(v) = sc.cache_get(&key) {
        return vs(&v);
    }
    let n = sc.db().first("select name from portal_user where id=:id", &row! {"id"=>id}).await.ok().flatten().map(|r| s(&r, "name")).unwrap_or_default();
    sc.cache_put(&key, json!(n));
    n
}

async fn node_name(sc: &Sc, id: i64) -> String {
    let key = format!("nname_{}", id);
    if let Some(v) = sc.cache_get(&key) {
        return vs(&v);
    }
    let n = sc.db().first("select name from portal_tree_node where id=:id", &row! {"id"=>id}).await.ok().flatten().map(|r| s(&r, "name")).unwrap_or_default();
    sc.cache_put(&key, json!(n));
    n
}

async fn field_options(sc: &Sc, f: &Row, datas: &Row) -> Vec<(String, String)> {
    let code = s(f, "field_options");
    if code.trim().is_empty() {
        return vec![];
    }
    option_pairs(&sc.eval_options(&code, &row! {"datas"=>Value::Object(datas.clone())}))
}

/// Human readable value of a field.
pub async fn display_field(sc: &Sc, t: &Tracker, f: &Row, value: &Value, datas: &Row, nolink: bool) -> String {
    let ft = s(f, "field_type");
    let fd = s(f, "field_display");
    if fd.contains("{{") || fd.contains("{%") {
        return sc.render(&fd, &row! {"value"=>value.clone(),"datas"=>Value::Object(datas.clone())}, true).unwrap_or_default();
    }
    if value.is_null() && ft != "HasMany" {
        return String::new();
    }
    let txt = vs(value);
    match ft.as_str() {
        "User" => match txt.parse::<i64>() {
            Ok(id) => esc(&user_name(sc, id).await),
            Err(_) => esc(&txt),
        },
        "TreeNode" | "Branch" => match txt.parse::<i64>() {
            Ok(id) => esc(&node_name(sc, id).await),
            Err(_) => esc(&txt),
        },
        "File" => match txt.parse::<i64>() {
            Ok(id) => file_display(sc, f, id).await,
            Err(_) => String::new(),
        },
        // as the Grails portal shows them: the stored value, or the field_format
        "Checkbox" => {
            let fmt = s(f, "field_format");
            if !fmt.is_empty() && !has(f, "field_options") {
                let labels = csv(&fmt);
                let on = matches!(txt.trim().to_lowercase().as_str(), "true" | "1" | "on" | "yes" | "y" | "t");
                esc(if on { labels.first().map(|x| x.as_str()).unwrap_or("") } else { labels.get(1).map(|x| x.as_str()).unwrap_or("") })
            } else {
                esc(&txt)
            }
        }
        "Date" | "DateTime" => {
            if txt.starts_with("1900-01-01") {
                return String::new();
            }
            let fmt = s(f, "field_format");
            match crate::script::parse_dt(&txt) {
                Some(d) if fmt.contains('%') => d.format(&fmt).to_string(),
                Some(_) if !fmt.trim().is_empty() => esc(&crate::groovy::java_date(&txt, &fmt)),
                // java.sql.Date / Timestamp toString
                Some(d) if ft == "Date" => d.format("%Y-%m-%d").to_string(),
                Some(d) => {
                    let frac = d.format("%.f").to_string();
                    format!("{}{}", d.format("%Y-%m-%d %H:%M:%S"), if frac.is_empty() { ".0".to_string() } else { frac.trim_end_matches('0').to_string() })
                }
                None => esc(&txt),
            }
        }
        "Number" | "Integer" => {
            let fmt = s(f, "field_format");
            if fmt.trim().is_empty() {
                esc(&txt)
            } else if let Ok(d) = fmt.trim().parse::<usize>() {
                fmt_number(value, d)
            } else {
                esc(&crate::groovy::number_format(value, &fmt))
            }
        }
        "BelongsTo" => {
            let Ok(oid) = txt.parse::<i64>() else { return esc(&txt) };
            let Some(ot) = t.other_tracker(sc.db(), f).await else { return esc(&txt) };
            let ff = other_format(f);
            // as the Grails portals: the referenced record's field (blank when the tracker has
            // no name/title field), and nothing at all when the record no longer exists
            let fname = if ff.is_empty() { ot.default_field().unwrap_or_default() } else { ff };
            let Some(od) = ot.get(sc.db(), oid).await else { return String::new() };
            let label = if fname.is_empty() { String::new() } else { s(&od, &fname) };
            if nolink || b(f, "suppress_follow_link") {
                esc(&label)
            } else {
                format!("<a href='/{}/{}/display/{}'>{}</a>", ot.module, ot.slug, oid, esc(&label))
            }
        }
        "HasMany" => String::new(),
        // the Grails portals' displayField shows a choice field's stored value as it is
        // a g6 portal's fieldval prints the stored text as it is (HTML and all), unless the
        // field is marked encode_exception: then escaped, with line breaks kept
        _ if crate::db::g6() && ft != "File" => {
            if b(f, "encode_exception") { esc(&txt).replace('\n', "<br/>") } else { txt }
        }
        "Text Area" => {
            if fd.trim() == "html" { txt } else { esc(&txt).replace('\n', "<br/>") }
        }
        _ => esc(&txt),
    }
}

/// A file field's value: a link to the file, or the image itself when field_display is `img[:width[:height]]`.
async fn file_display(sc: &Sc, f: &Row, id: i64) -> String {
    let Ok(Some(fl)) = sc.db().get("file_link", id).await else { return String::new() };
    let fd = s(f, "field_display");
    let parts: Vec<&str> = fd.split(':').collect();
    if parts.first() == Some(&"img") {
        let mut style = String::new();
        if let Some(w) = parts.get(1) {
            style = format!(" style='width:{}{}'", esc(w), parts.get(2).map(|h| format!(";height:{}", esc(h))).unwrap_or_default());
        }
        return format!("<img src='/fileLink/get/{}?inline=1'{}/>", id, style);
    }
    format!("<a href=\"/fileLink/get/{}\">{}</a>", id, esc(&s(&fl, "name")))
}

/// `<div id='name_value_div'>value</div>`, the wrapper every displayed value has in the Grails portal.
pub async fn value_div(sc: &Sc, t: &Tracker, f: &Row, value: &Value, datas: &Row, nolink: bool) -> String {
    format!("<div id='{}_value_div'>{}</div>", esc(s(f, "name").trim()), display_field(sc, t, f, value, datas, nolink).await)
}

// ---------------- filter ----------------

pub async fn tracker_filter(sc: &Sc, t: &Tracker) -> String {
    // Same behaviour as the Grails portal's trackerFilter tag: every filter field is a
    // dropdown of the values present in the data (dates get a Before/After/Between
    // picker), changing a dropdown submits the form, and other list parameters are kept.
    let p = &sc.inner.params;
    let has_search = has(&t.row, "searchfields");
    let fields: Vec<&Row> = csv(&s(&t.row, "filterfields")).iter().filter_map(|n| t.field(n)).collect();
    if fields.is_empty() && !has_search {
        return String::new();
    }
    let table = t.data_table();
    let trackers_objects = crate::web::setting(sc.db(), "tracker_objects").await.unwrap_or(Value::Null);
    let mut keep: Vec<String> = vec!["module".into(), "slug".into(), "offset".into(), "action".into(), "controller".into()];
    let mut o = format!("<form method='get' action='{}' id='trackfilter' name='trackfilter' class='tracker_filter'><fieldset class='form'>", t.url("list"));
    let opt = |v: &str, label: &str, cur: &str| format!("<option value='{}'{}>{}</option>", esc(v), if !cur.is_empty() && cur.trim() == v.trim() { " selected" } else { "" }, esc(label));
    for f in fields {
        let name = s(f, "name").trim().to_string();
        let cur = s(p, &name);
        let ftype = s(f, "field_type");
        o.push_str(&format!("<div class='fieldcontain'><label>{}</label>", esc(&s(f, "label"))));
        keep.push(name.clone());
        if ftype == "Date" || ftype == "DateTime" {
            let fid = format!("date_filter_type_{}", name);
            let kind = s(p, &fid);
            o.push_str(&format!("<input type='hidden' name='{n}' id='{n}' value='{}'/>", esc(&cur), n = esc(&name)));
            o.push_str(&format!("<select class='datefilter' id='{f}' name='{f}'>", f = fid));
            for k in ["Before", "After", "Between"] {
                o.push_str(&format!("<option{}>{}</option>", if kind == k { " selected" } else { "" }, k));
            }
            o.push_str("</select>");
            o.push_str(&format!(" <input type='date' class='{n}_date' id='{n}_first' name='{n}_first' value='{}'/>", esc(&s(p, &format!("{}_first", name))), n = esc(&name)));
            o.push_str(&format!(" <span id='{n}_second_span'{}>- <input class='{n}_date' type='date' id='{n}_second' name='{n}_second' value='{}'/></span>", if kind != "Between" { " style='display:none'" } else { "" }, esc(&s(p, &format!("{}_second", name))), n = esc(&name)));
            o.push_str(&format!(
                "<script>$('#{f}').on('change',function(){{var df=$(this).val();if(df=='Before'||df=='After'){{$('#{n}_second_span').hide();}}else{{$('#{n}_second_span').show();}}}});\
$('.{n}_date').on('change',function(){{var df=$('#{f}').val(),a=$('#{n}_first').val(),b=$('#{n}_second').val();\
if(df=='Before'){{$('#{n}').val(a?('<'+a):'');}}else if(df=='After'){{$('#{n}').val(a?('>'+a):'');}}else{{$('#{n}').val((a&&b)?('between_'+a+'_'+b):'');}}}});</script>",
                f = fid, n = name
            ));
            keep.push(fid);
            keep.push(format!("{}_first", name));
            keep.push(format!("{}_second", name));
            o.push_str("</div>");
            continue;
        }
        o.push_str(&format!("<select class='filterdropdown' name='{n}' id='{n}'><option value=''></option>", n = esc(&name)));
        let distinct = format!("select distinct {c} as v from {tb} order by {c}", c = qcol(&name), tb = table);
        if name == "record_status" {
            let mut st: Vec<String> = t.statuses.iter().map(|x| s(x, "name")).filter(|n| n != "Delete").collect();
            st.sort();
            for n in st {
                o.push_str(&opt(&n, &n, &cur));
            }
        } else if let Some(spec) = trackers_objects.get(&ftype).map(vs) {
            // a field type mapped to a tracker in the tracker_objects setting
            let parts: Vec<&str> = spec.split('.').map(|x| x.trim()).collect();
            let other = if parts.len() > 1 { Tracker::load(sc.db(), parts[0], parts[1]).await } else { Tracker::load_ref(sc.db(), parts[0], &t.module).await };
            if let Some(ot) = other {
                let shown = if parts.len() == 3 { parts[2].to_string() } else { ot.default_field().unwrap_or_else(|| "id".into()) };
                for r in sc.db().rows(&distinct, &Row::new()).await.unwrap_or_default() {
                    let v = s(&r, "v");
                    if let Ok(id) = v.trim().parse::<i64>() {
                        if let Some(d) = ot.get(sc.db(), id).await {
                            o.push_str(&opt(&v, &s(&d, &shown), &cur));
                        }
                    }
                }
            }
        } else if ftype == "User" {
            let q = format!("select d.v, u.name from (select distinct {c} as v from {tb}) d left join portal_user u on u.id::text = d.v::text order by d.v", c = qcol(&name), tb = table);
            for r in sc.db().rows(&q, &Row::new()).await.unwrap_or_default() {
                o.push_str(&opt(&s(&r, "v"), &s(&r, "name"), &cur));
            }
        } else if ftype == "Drop Down" {
            for (v, _) in field_options(sc, f, &Row::new()).await {
                o.push_str(&opt(&v, &v, &cur));
            }
        } else if ftype == "BelongsTo" {
            match t.other_tracker(sc.db(), f).await {
                Some(ot) => {
                    let ff = other_format(f);
                    let q = if ff.is_empty() { format!("select id, id as l from {} order by id", ot.data_table()) } else { format!("select id, {c} as l from {} order by {c}", ot.data_table(), c = qcol(&ff)) };
                    for r in sc.db().rows(&q, &Row::new()).await.unwrap_or_default() {
                        o.push_str(&opt(&s(&r, "id"), &s(&r, "l"), &cur));
                    }
                }
                None => {
                    for r in sc.db().rows(&distinct, &Row::new()).await.unwrap_or_default() {
                        o.push_str(&opt(&s(&r, "v"), &s(&r, "v"), &cur));
                    }
                }
            }
        } else {
            for r in sc.db().rows(&distinct, &Row::new()).await.unwrap_or_default() {
                let v = s(&r, "v");
                o.push_str(&opt(&v, &v, &cur));
            }
        }
        o.push_str("</select></div>");
    }
    if has_search {
        o.push_str(&format!("<div class='fieldcontain' id='tracksearch'><label>Search:</label><input type='text' name='search' id='search' value='{}'/><input type='submit' name='dosearch' class='save' value='Search' id='dosearch'/></div>", esc(&s(p, "search"))));
        keep.push("search".into());
        keep.push("dosearch".into());
    }
    keep.push("csrf_token".into());
    for (k, v) in p {
        if !keep.contains(k) && k != &k.to_uppercase() {
            o.push_str(&format!("<input type='hidden' name='{}' value='{}'/>", esc(k), esc(&vs(v))));
        }
    }
    o.push_str("</fieldset><br/></form><script>$('.filterdropdown').change(function(){ $('#trackfilter').submit(); });</script>");
    o
}

// ---------------- list ----------------

/// Grails' `<g:paginate>`: Previous, a window of 10 steps with the first and last
/// page, Next. `base` is the page URL; other request parameters are kept.
pub fn paginate(base: &str, params: &Row, total: i64, max: i64, offset: i64) -> String {
    let max = max.max(1);
    let q = web::qs(params, &["max", "offset", "module", "slug", "csrf_token"]);
    let href = |off: i64| format!("{}?max={}&amp;offset={}{}", base, max, off, if q.is_empty() { String::new() } else { format!("&amp;{}", esc(&q)) });
    let maxsteps = 10i64;
    let current = offset / max + 1;
    let last = (total + max - 1) / max;
    let mut o = String::new();
    if current > 1 {
        o.push_str(&format!("<a href=\"{}\" class=\"prevLink\">Previous</a>", href((offset - max).max(0))));
    }
    if last > 1 {
        let mut begin = current - maxsteps / 2 + (maxsteps % 2);
        let mut end = current + maxsteps / 2 - 1;
        if begin < 1 {
            begin = 1;
            end = maxsteps;
        }
        if end > last {
            begin = (last - maxsteps + 1).max(1);
            end = last;
        }
        if begin > 1 {
            o.push_str(&format!("<a href=\"{}\" class=\"step\">1</a>", href(0)));
        }
        if begin > 2 {
            o.push_str("<span class=\"step gap\">..</span>");
        }
        for p in begin..=end {
            if p == current {
                o.push_str(&format!("<span class=\"currentStep\">{}</span>", p));
            } else {
                o.push_str(&format!("<a href=\"{}\" class=\"step\">{}</a>", href((p - 1) * max), p));
            }
        }
        if end < last - 1 {
            o.push_str("<span class=\"step gap\">..</span>");
        }
        if end < last {
            o.push_str(&format!("<a href=\"{}\" class=\"step\">{}</a>", href((last - 1) * max), last));
        }
    }
    if current < last {
        o.push_str(&format!("<a href=\"{}\" class=\"nextLink\">Next</a>", href(offset + max)));
    }
    o
}

/// "Role->Edit,Approve; Other->Close" or just "Edit,Approve".
fn action_names(spec: &str, roles: &[String]) -> Vec<String> {
    let mut out = vec![];
    for part in spec.split(';') {
        match part.split_once("->") {
            Some((who, acts)) => {
                if csv(who).iter().any(|w| roles.contains(w)) {
                    out.extend(csv(acts));
                }
            }
            None => out.extend(csv(part)),
        }
    }
    out
}

pub async fn tracker_list(sc: &Sc, t: &Tracker, opts: &Row) -> String {
    let db = sc.db();
    let mut params = sc.inner.params.clone();
    for (k, v) in opts {
        if k != "id" && k != "transition" {
            params.insert(k.clone(), v.clone());
        }
    }
    let names = { let l = csv(&s(&t.row, "listfields")); if l.is_empty() { t.fields.iter().map(|f| s(f, "name")).take(8).collect() } else { l } };
    let mut fields: Vec<Row> = vec![];
    for n in &names {
        if let Some(f) = t.field(n) {
            fields.push(f.clone());
        } else if n == "row_number" {
            fields.push(row! {"name"=>"row_number","label"=>"No.","field_type"=>"Row"});
        }
        // anything else that is not one of the tracker's fields (e.g. record_status,
        // id) is left out, as the Grails portals' trackerList does
    }
    let default_max = i(&t.row, "defaultlimit").filter(|x| *x > 0).unwrap_or(10);
    let max = i(&params, "max").filter(|x| *x > 0).unwrap_or(default_max).min(1000);
    let offset = i(&params, "offset").unwrap_or(0).max(0);
    params.insert("max".into(), json!(max));
    params.insert("offset".into(), json!(offset));
    let total = db.count(&t.list_query(sc, &params, "select count(*)", false).await, &db::no_params()).await.unwrap_or(0);
    let rows = match db.rows(&t.list_query(sc, &params, "select *", false).await, &db::no_params()).await {
        Ok(r) => r,
        Err(e) => {
            web::log_error(db, "tracker", "list", &e.to_string(), Some(&t.module), Some(&t.slug), None).await;
            vec![]
        }
    };
    let actions = s(&t.row, "actionbuttons");
    let rowclass = s(&t.row, "rowclassval");
    // same markup as the Grails portal's trackerList tag (styled by its main.css/mobile.css)
    let mut o = String::from("\n        <style>\n        \t@media\n\t  only screen \n    and (max-width: 760px), (min-device-width: 768px) \n    and (max-device-width: 1024px)  { ");
    for (n, f) in fields.iter().enumerate() {
        o.push_str(&format!("td:nth-of-type({}):before {{ content: '{}'; }}", n + 1, s(f, "label").replace('\'', "\\'").replace('<', "")));
    }
    o.push_str("\n    }\n        </style>\n        <table class='responsive'><thead><tr>");
    for f in &fields {
        let l = s(f, "label");
        o.push_str(&format!("<th id='col_{}'>{}&nbsp;</th>", esc(&l.replace(' ', "_").to_lowercase()), esc(&l)));
    }
    if !actions.is_empty() {
        o.push_str("<th>Action</th>");
    }
    o.push_str("</tr></thead>");
    for (n, r) in rows.iter().enumerate() {
        let st = if t.tracker_type == "DataStore" { String::new() } else { s(r, "record_status").replace(' ', "_").to_lowercase() };
        let mut cls = String::new();
        if !rowclass.trim().is_empty() {
            cls = sc.eval(&rowclass, &row! {"row"=>Value::Object(r.clone()),"datas"=>Value::Object(r.clone())}).map(|v| vs(&v)).unwrap_or_default();
        }
        o.push_str(&format!("<tr class='status_{} {}'>", esc(if st.is_empty() { "default" } else { &st }), esc(&cls)));
        let rid = i(r, "id").unwrap_or(0);
        for (pos, f) in fields.iter().enumerate() {
            let fname = s(f, "name");
            let ft = s(f, "field_type");
            let tdc = if ft == "Integer" || ft == "Number" { " numbercell" } else { "" };
            o.push_str(&format!("<td class='{}'>", tdc));
            let cell = if ft == "Row" {
                format!("<div id='row_number_value_div'>{}</div>", offset + n as i64 + 1)
            } else if ft == "HasMany" {
                format!("<div id='{}_value_div'></div>", esc(&fname))
            } else {
                value_div(sc, t, f, r.get(&fname).unwrap_or(&Value::Null), r, pos == 0).await
            };
            if pos == 0 {
                let empty = cell.ends_with("_value_div'></div>");
                o.push_str(&format!("<a href=\"{}/{}\" class=\"linkdetail \">{}</a>", t.url("display"), rid, if empty { " - ".to_string() } else { cell }));
            } else {
                o.push_str(&cell);
            }
            o.push_str("&nbsp;</td>");
        }
        if !actions.is_empty() && sc.inner.user.is_some() {
            o.push_str("<td>");
            let uroles = t.user_roles(sc, Some(r)).await;
            let wanted = action_names(&actions, &uroles);
            let mut any = false;
            for tr in t.available_transitions(sc, r).await {
                if wanted.iter().any(|w| w.eq_ignore_ascii_case(&tr.name)) {
                    any = true;
                    o.push_str(&format!("<a href=\"/{}/{}/{}/{}\" class=\"btn btn-primary m-1\" name=\"transition_{}\">{}</a>", t.module, t.slug, tr.url_name(), rid, tr.id, esc(&tr.name)));
                }
            }
            if !any {
                o.push_str("&nbsp;");
            }
            o.push_str("</td>");
        }
        o.push_str("</tr>");
    }
    o.push_str("</table>");
    if total > 10 {
        let base = if sc.inner.path.is_empty() { t.url("list") } else { sc.inner.path.clone() };
        o.push_str(&format!("<div class='pagination'>{}</div>", paginate(&base, &sc.inner.params, total, max, offset)));
    }
    o
}

// ---------------- field error checks ----------------

/// Evaluate a field's error checks against submitted values.
/// Returns (message, fatal) pairs.
pub async fn field_errors(sc: &Sc, t: &Tracker, f: &Row, datas: &Row) -> Vec<(String, bool)> {
    let checks = sc.db().rows("select * from portal_tracker_error where field_id=:f order by id", &row! {"f"=>i(f, "id")}).await.unwrap_or_default();
    let name = s(f, "name");
    let val = s(datas, &name);
    let label = s(f, "label");
    let mut out = vec![];
    for c in checks {
        let failed: Option<String> = match s(&c, "error_type").as_str() {
            "Not Empty" => if val.trim().is_empty() { Some(if crate::db::g6() { format!("{} cannot be empty", label) } else { format!("{} cannot be empty", label) }) } else { None },
            "Unique" => {
                if val.trim().is_empty() {
                    None
                } else {
                    let q = format!("select count(*) from {} where {}::text = :v and id != :id and coalesce(record_status,'')!='sys_draft'", qcol(&t.data_table()), qcol(&name));
                    let q = if t.tracker_type == "DataStore" { q.replace(" and coalesce(record_status,'')!='sys_draft'", "") } else { q };
                    let n = sc.db().count(&q, &row! {"v"=>val.clone(),"id"=>i(datas, "id").unwrap_or(0)}).await.unwrap_or(0);
                    if n > 0 { Some(if crate::db::g6() { format!("{} already exists", label) } else { format!("{} must be unique", label) }) } else { None }
                }
            }
            "Format" => {
                let pat = s(&c, "format");
                if val.is_empty() || pat.is_empty() {
                    None
                } else {
                    match regex::Regex::new(&format!("^(?:{})$", pat)) {
                        Ok(re) if !re.is_match(&val) => Some(if crate::db::g6() { format!("{} need to be in the form of {}", label, pat) } else { format!("{} is not in the right format", label) }),
                        _ => None,
                    }
                }
            }
            "E-mail" => {
                let re = regex::Regex::new(r"^[^@\s]+@[^@\s]+\.[^@\s]+$").unwrap();
                if !val.is_empty() && !re.is_match(val.trim()) { Some(if crate::db::g6() { format!("{} need to be a proper e-mail address", label) } else { format!("{} must be a valid e-mail address", label) }) } else { None }
            }
            "Custom" => {
                let code = s(&c, "error_function");
                if code.trim().is_empty() {
                    None
                } else {
                    let eval_code = if crate::db::g6() || crate::tracker::looks_groovy_snippet(&code) {
                        crate::tracker::groovy_tracker_script(&code)
                    } else {
                        code
                    };
                    match sc.eval(&eval_code, &row! {"datas"=>Value::Object(datas.clone()),"field"=>Value::Object(f.clone()),"value"=>val.clone(),"val"=>val.clone()}) {
                        Ok(v) if crate::script::truthy(&v) => Some(if v == json!(true) { format!("{} is invalid", label) } else { vs(&v) }),
                        Ok(_) => None,
                        Err(e) => {
                            web::log_error(sc.db(), "trackerField", "error_function", &e.to_string(), Some(&t.module), Some(&t.slug), None).await;
                            None
                        }
                    }
                }
            }
            _ => None,
        };
        if let Some(default_msg) = failed {
            let custom = s(&c, "error_msg");
            let custom_empty = custom.trim().is_empty();
            let msg = if custom.trim().is_empty() || s(&c, "error_type") == "Custom" && custom.trim().is_empty() {
                default_msg
            } else {
                // a Grails portal's message is a Groovy expression, usually a quoted string
                let evaluated = match crate::tracker::groovy_default(&custom) {
                    crate::tracker::GroovyDefault::Value(v) => Ok(v),
                    crate::tracker::GroovyDefault::Code(code) => sc.eval(&code, &row! {"datas"=>Value::Object(datas.clone()),"field"=>Value::Object(f.clone()),"value"=>val.clone()}),
                };
                match evaluated {
                    Ok(v) if !vs(&v).is_empty() => vs(&v),
                    _ => custom,
                }
            };
            // a g6 portal ends each message with a space (they are joined with "; ")
            let msg = if crate::db::g6() && !(s(&c, "error_type") == "Format" && custom_empty) { format!("{} ", msg) } else { msg };
            out.push((msg, !b(&c, "allow_submission")));
        }
    }
    out
}

pub fn errors_html(errs: &[(String, bool)]) -> String {
    errs.iter().map(|(m, fatal)| format!("<span class='{}'>{}</span> ", if *fatal { "fatal_error text-danger" } else { "warning_error text-warning" }, esc(m))).collect()
}

// ---------------- form ----------------

const FORM_JS: &str = r#"<script>
function toggleAll(name, show){
  var tr = document.getElementById(name + '_tr');
  if(!tr) return;
  tr.style.display = show ? '' : 'none';
  tr.querySelectorAll('input,select,textarea').forEach(function(el){
    if(!show){ if(el.required){ el.dataset.wasRequired = '1'; el.required = false; } }
    else if(el.dataset.wasRequired){ el.required = true; }
  });
}
function update_submit(){
  var blocked = false;
  document.querySelectorAll('.fatal_error').forEach(function(el){
    if(el.offsetParent !== null && el.textContent.trim().length) blocked = true;
  });
  var sb = document.getElementById('submit');
  if(sb) sb.disabled = blocked;
}
document.addEventListener('htmx:afterSwap', function(){ update_submit(); });
$(function(){
  $('select.ajax-select').each(function(){
    var el = $(this);
    el.select2({ width: 'style', allowClear: true, placeholder: '', dropdownParent: el.closest('.fieldcontain').length ? el.closest('.fieldcontain') : $(document.body), ajax: { url: el.data('url'), dataType: 'json', delay: 250,
      data: function(p){ return { q: p.term }; } } });
  });
});
</script>"#;

pub fn form_js() -> &'static str {
    FORM_JS
}

/// One form field, as the Grails portal's trackerField tag writes it:
/// `<div class='group fieldcontain' id='x_div'><label/><div class='field-input-col'>input <span/></div></div>`.
fn field_html<'a>(sc: &'a Sc, t: &'a Tracker, tr: &'a Trans, f: &'a Row, datas: &'a Row, id: Option<i64>, required: &'a [String], zindex: i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        let p = &sc.inner.params;
        let name = s(f, "name").trim().to_string();
        // the 'slug' field is posted as indata_slug ('slug' is the URL's tracker slug)
        let n = if name == "slug" { "indata_slug".to_string() } else { esc(&name) };
        let ft = s(f, "field_type");
        let label = s(f, "label");
        let tip = if has(f, "field_tooltip") { format!(" title='{}'", esc(&s(f, "field_tooltip"))) } else { String::new() };
        if ft == "FieldGroup" {
            let mut o = format!("<fieldset><legend{}>{}</legend>", tip, esc(&label));
            if has(f, "field_description") {
                o.push_str(&format!("<small class='field-description text-muted d-block mb-2'>{}</small>", esc(&s(f, "field_description"))));
            }
            for m in csv(&s(f, "field_options")) {
                if let Some(mf) = t.field(&m) {
                    o.push_str(&field_html(sc, t, tr, mf, datas, id, required, zindex).await);
                }
            }
            o.push_str("</fieldset>");
            return o;
        }
        // the value: what was just posted (redisplay after an error), the record, or the URL
        let redisplay = p.contains_key("submit");
        let mut value = datas.get(&name).cloned().filter(|v| !v.is_null() && vs(v) != "").unwrap_or(Value::Null);
        if (redisplay && p.contains_key(&name)) || (value.is_null() && b(f, "url_value") && has(p, &name)) {
            value = p.get(&name).cloned().unwrap_or(Value::Null);
        }
        if has(f, "field_default") {
            if !crate::db::g6() {
                return String::new(); // worked out on the server when the form is saved
            }
            // a g6 portal evaluates the default here: a value goes in a hidden field,
            // nothing means the ordinary input is shown
            let vars = crate::row! {"datas"=>Value::Object(datas.clone())};
            let dv = match crate::tracker::groovy_default(&s(f, "field_default")) {
                crate::tracker::GroovyDefault::Value(v) => Some(v),
                crate::tracker::GroovyDefault::Code(c) => sc.eval(&c, &vars).ok(),
            };
            match dv {
                Some(Value::Null) => {}
                Some(v) => return format!("<input type=\"hidden\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&vs(&v))),
                None => return format!("<input type=\"hidden\" name=\"{n}\" value=\"-1\" id=\"{n}\" />"),
            }
        }
        if b(f, "params_override") && has(p, &name) {
            return format!("<input type=\"hidden\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&s(p, &name)));
        }
        if ft == "Hidden" {
            return format!("<input type=\"hidden\" name=\"{n}\" value=\"{}\" id=\"{n}\" />", esc(&vs(&value)));
        }
        let v = vs(&value);
        // a g6 portal checks required fields with its own scripts, not the browser's
        let req = if required.contains(&name) && !crate::db::g6() { " required" } else { "" };
        let ph = if has(f, "field_placeholder") { format!(" placeholder='{}'", esc(&s(f, "field_placeholder"))) } else { String::new() };
        let hs = if !has(f, "hyperscript") {
            String::new()
        } else if crate::db::g6() {
            // a g6 portal evaluates the column as Groovy and puts the result in the tag
            // as raw attributes (a triple-quoted string literal holding oninput="...")
            format!(" {}", groovy_literal(&s(f, "hyperscript")))
        } else {
            format!(" _=\"{}\"", esc(&s(f, "hyperscript")))
        };
        let mut classes = s(f, "classes");
        if ft == "MultiSelect" {
            classes.push_str(" multidiv");
        }
        let child_link = ft == "BelongsTo" && has(p, &name);
        // a g6 portal names the wrapper after the field itself (slug_div, not indata_slug_div)
        let div_id = if crate::db::g6() { esc(&name) } else { n.clone() };
        let mut o = format!("<div class='group fieldcontain {}' id='{}_div'  style='z-index:{}'  >", esc(&classes), div_id, zindex);
        let g6 = crate::db::g6();
        if !child_link && !b(f, "hide_heading") && g6 {
            // a g6 portal's form: the label alone (no description, no tooltip)
            o.push_str(&format!("<label for='{}'>{}</label>", esc(&name), esc(&label)));
        } else if !child_link && !b(f, "hide_heading") {
            if has(f, "field_description") {
                o.push_str(&format!("<div class='field-label-col'><label for='{n}'{}>{}</label><small class='field-description text-muted'>{}</small></div>", tip, esc(&label), esc(&s(f, "field_description"))));
            } else {
                o.push_str(&format!("<label for='{n}'{}>{}</label>", tip, esc(&label)));
            }
        }
        if !g6 {
            o.push_str("<div class='field-input-col'>");
        }
        let select = |opts: Vec<(String, String)>, cur: &str, empty: Option<&str>| -> String {
            let mut x = format!("<select name=\"{n}\" id=\"{n}\"{}{} >", req, hs);
            if let Some(e) = empty {
                x.push_str(&format!("<option value=\"\">{}</option>", esc(e)));
            }
            for (ov, ol) in opts {
                x.push_str(&format!("<option value=\"{}\"{} >{}</option>", esc(&ov), if ov == cur { " selected=\"selected\"" } else { "" }, esc(&ol)));
            }
            x.push_str("</select>");
            x
        };
        let input = match ft.as_str() {
            "Text Area" => format!("<textarea name=\"{n}\" id=\"{n}\"{}{}{} >{}</textarea>", ph, req, hs, esc(&v)),
            "Integer" => format!("<input type='number' id='{n}' name='{n}' {}{}{}{}/>", if v.is_empty() { String::new() } else { format!("value='{}'", esc(&v)) }, ph, req, hs),
            "Number" => {
                let fmt = s(f, "field_format");
                let step = match fmt.split_once('.') {
                    Some((_, d)) if !d.is_empty() => format!("0.{}1", "0".repeat(d.len().saturating_sub(1))),
                    _ => "any".to_string(),
                };
                format!("<input type='number' id='{n}' name='{n}'  step='{}' {}{}{}{}/>", step, if v.is_empty() { String::new() } else { format!("value='{}'", esc(&v)) }, ph, req, hs)
            }
            "Date" => {
                let d = if v.starts_with("1900-01-01") { String::new() } else { v.chars().take(10).collect() };
                // a g6 portal prints an empty date as value='null'
                let d = if d.is_empty() && crate::db::g6() { "null".to_string() } else { d };
                format!("<input type='date' id='{n}' name='{n}' value='{}'{}{} />", esc(&d), req, hs)
            }
            "DateTime" => {
                let d: String = v.replace(' ', "T").chars().take(16).collect();
                let d = if d.is_empty() && crate::db::g6() { "null".to_string() } else { d };
                format!("<input type='datetime-local' id='{n}' name='{n}' value='{}'{}{} />", esc(&d), req, hs)
            }
            "Checkbox" if has(f, "field_options") => {
                let cur = csv(&v);
                let mut x = String::new();
                for (ov, ol) in field_options(sc, f, datas).await {
                    let lid = format!("{}{}", ov, name).to_lowercase().replace(' ', "_");
                    x.push_str(&format!("<label for='{}'>{} :</label><input type=\"hidden\" name=\"_{n}\" /><input type=\"checkbox\" name=\"{n}\" value=\"{}\"{} id=\"{}\" /><br/>", esc(&lid), esc(&ol), esc(&ov), if cur.contains(&ov) { " checked=\"checked\"" } else { "" }, esc(&lid)));
                }
                x
            }
            "Checkbox" => {
                let on = value == json!(true) || matches!(v.to_lowercase().as_str(), "on" | "1" | "true" | "t" | "yes");
                format!("<input type=\"hidden\" name=\"_{n}\" /><input type=\"checkbox\" name=\"{n}\"{} id=\"{n}\"{} />", if on { " checked=\"checked\"" } else { "" }, hs)
            }
            "MultiSelect" => {
                let cur: Vec<String> = match &value {
                    Value::Array(a) => a.iter().map(vs).collect(),
                    _ => csv(&v),
                };
                let mut x = String::from("<div class='multiselect'>");
                for (ov, ol) in field_options(sc, f, datas).await {
                    let lid = format!("{}{}", ov, name).to_lowercase().replace(' ', "_");
                    x.push_str(&format!("<span class='multichoice'><label for='{}'>{} :</label><input type=\"hidden\" name=\"_{n}\" /><input type=\"checkbox\" name=\"{n}\" value=\"{}\"{} id=\"{}\" /></span><br/>", esc(&lid), esc(&ol), esc(&ov), if cur.contains(&ov) { " checked=\"checked\"" } else { "" }, esc(&lid)));
                }
                x.push_str("</div>");
                x
            }
            "File" => format!("<input type='file' name='{n}' id='{n}'{}{} />", if req.is_empty() || !v.is_empty() { "" } else { " required" }, hs),
            "Drop Down" => {
                let opts = field_options(sc, f, datas).await;
                let many = opts.len() > 20;
                let mut x = select(opts, &v, None);
                if many {
                    x.push_str(&format!("<script>$(function(){{ $('#{n}').select2({{ dropdownParent: $('#{n}_div') }}); }});</script>"));
                }
                x
            }
            "BelongsTo" if child_link => {
                // creating a child record from its parent: the link is fixed
                format!(
                    "<input type=\"hidden\" name=\"backtoid\" value=\"{}\" id=\"backtoid\" /><input type=\"hidden\" name=\"{n}\" value=\"{}\" id=\"{n}\" /><input type=\"hidden\" name=\"backto\" value=\"{}\" id=\"backto\" /><input type=\"hidden\" name=\"backtransition\" value=\"{}\" id=\"backtransition\" />",
                    esc(&s(p, &name)), esc(&s(p, &name)), esc(&s(f, "field_options")), esc(&s(p, "backtransition"))
                )
            }
            "BelongsTo" => match t.other_tracker(sc.db(), f).await {
                Some(ot) => {
                    let ff = other_format(f);
                    let fname = if ff.is_empty() { ot.default_field().unwrap_or_else(|| "id".into()) } else { ff };
                    let total = ot.count(sc.db(), None).await.unwrap_or(0);
                    if crate::db::g6() {
                        // a g6 portal lists every row of the other tracker, in table order; past
                        // its select2 threshold (default 10) with "Please select" and a search box
                        let threshold = web::setting_str(sc.db(), "select2_threshold").await.and_then(|x| x.trim().parse::<usize>().ok()).unwrap_or(10);
                        let q = format!("select id, {} as label from {}", qcol(&fname), qcol(&ot.data_table()));
                        let rows = sc.db().rows(&q, &crate::db::no_params()).await.unwrap_or_default();
                        let opts = rows.iter().map(|r| (s(r, "id"), s(r, "label"))).collect::<Vec<_>>();
                        let many = opts.len() > threshold;
                        let mut x = select(opts, &v, if many { Some("Please select") } else { None });
                        if many {
                            x.push_str(&format!("<script>$(function(){{ $('#{n}').select2({{ dropdownParent: $('#{n}_div') }}); }});</script>"));
                        }
                        x
                    } else if total > 300 {
                        // too many to list: search as you type
                        let mut opt = String::from("<option value=\"\">Please select</option>");
                        if let Ok(oid) = v.parse::<i64>() {
                            if let Some(od) = ot.get(sc.db(), oid).await {
                                opt.push_str(&format!("<option value=\"{}\" selected=\"selected\">{}</option>", oid, esc(&s(&od, &fname))));
                            }
                        }
                        format!("<select name=\"{n}\" id=\"{n}\" class=\"ajax-select\" data-url=\"/portalTracker/objectlist?id={}\"{}{} >{}</select>", i(f, "id").unwrap_or(0), req, hs, opt)
                    } else {
                        let rows = ot.rows(sc.db(), None, Some(&qcol(&fname)), None, None).await.unwrap_or_default();
                        let opts = rows.iter().map(|r| (s(r, "id"), s(r, &fname))).collect::<Vec<_>>();
                        let mut x = select(opts, &v, if total > 20 { Some("Please select") } else { None });
                        if total > 20 {
                            x.push_str(&format!("<script>$(function(){{ $('#{n}').select2({{ dropdownParent: $('#{n}_div') }}); }});</script>"));
                        }
                        x
                    }
                }
                None => format!("<input type='number' id='{n}' name='{n}' value='{}'{} />", esc(&v), req),
            },
            "User" | "TreeNode" | "Branch" => {
                let (url, lbl) = match ft.as_str() {
                    "User" => ("userlist", match v.parse::<i64>() { Ok(x) => user_name(sc, x).await, Err(_) => String::new() }),
                    _ => ("nodeslist", match v.parse::<i64>() { Ok(x) => node_name(sc, x).await, Err(_) => String::new() }),
                };
                let opt = if lbl.is_empty() { String::new() } else { format!("<option value='{}' selected>{}</option>", esc(&v), esc(&lbl)) };
                format!("<select name='{n}' id='{n}' style='width: 40%;' class='ajax-select' data-url='/portalTracker/{}?id={}'{}{}>{}</select>", url, i(f, "id").unwrap_or(0), req, hs, opt)
            }
            "HasMany" => match id {
                Some(rid) => has_many(sc, t, f, rid, Some(tr)).await,
                None => String::new(),
            },
            _ => format!("<input type='text' id='{n}' name='{n}' {}{}{}{}/>", if v.is_empty() { String::new() } else { format!("value='{}'", esc(&v)) }, ph, req, hs),
        };
        o.push_str(&input);
        // error checks: re-run on the server as the user types (HTMX)
        let has_checks = sc.db().count("select count(*) from portal_tracker_error where field_id=:f", &row! {"f"=>i(f, "id")}).await.unwrap_or(0) > 0;
        if g6 {
            // the current messages are shown straight away; the span replaces itself as
            // the user types
            let hx = if has_checks {
                let trig = if matches!(ft.as_str(), "Date" | "DateTime" | "Drop Down" | "File") { format!("change from:#{n}") } else { format!("change from:#{n}, keyup from:#{n} changed delay:500ms") };
                format!("hx-trigger='{}' hx-post='/portalTrackerField/onchange?hx_field_id={}' hx-swap='outerHTML' hx-target='this' ", trig, i(f, "id").unwrap_or(0))
            } else {
                String::new()
            };
            let errs = if has_checks { field_errors(sc, t, f, datas).await } else { vec![] };
            let fatal = errs.iter().any(|(_, x)| *x);
            o.push_str(&format!(" &nbsp;<span {} {}>{}</span>", hx, if fatal { " class='text-danger fatal_error' " } else { "" }, esc(&errs.iter().map(|(m, _)| m.clone()).collect::<Vec<_>>().join("; "))));
            o.push_str("</div>");
        } else {
            if has_checks {
                let trig = if matches!(ft.as_str(), "Date" | "DateTime" | "Drop Down" | "File") { format!("change from:#{n}") } else { format!("change from:#{n}, keyup from:#{n} changed delay:500ms") };
                o.push_str(&format!(" &nbsp;<span id='{n}_error' hx-trigger='{}' hx-post='/portalTrackerField/onchange?hx_field_id={}' hx-include='closest form' hx-swap='innerHTML' hx-target='this' ></span>", trig, i(f, "id").unwrap_or(0)));
            } else {
                o.push_str(" &nbsp;<span ></span>");
            }
            o.push_str("</div></div>");
        }
        o
    })
}

/// Read-only fields shown above a transition form (the transition's displayfields).
pub async fn tracker_info(sc: &Sc, t: &Tracker, tr: &Trans, id: Option<i64>) -> String {
    let datas = match id {
        Some(rid) => t.get(sc.db(), rid).await.unwrap_or_default(),
        None => Row::new(),
    };
    let mut o = String::from("<table class='requestview'>");
    for n in csv(&s(&tr.row, "displayfields")) {
        if let Some(f) = t.field(&n) {
            o.push_str(&field_row(sc, t, f, &datas, id.unwrap_or(0)).await);
        }
    }
    o.push_str("</table>");
    o
}

pub async fn tracker_form(sc: &Sc, t: &Tracker, tr: &Trans, id: Option<i64>, opts: &Row) -> String {
    let p = &sc.inner.params;
    let datas = match id {
        Some(rid) => t.get(sc.db(), rid).await.unwrap_or_default(),
        None => Row::new(),
    };
    let required = csv(&s(&tr.row, "requiredfields"));
    let action = match id {
        Some(rid) => format!("/{}/{}/{}/{}", t.module, t.slug, tr.url_name(), rid),
        None => format!("/{}/{}/{}", t.module, t.slug, tr.url_name()),
    };
    let fid = format!("{}_form", t.slug);
    let mut o = format!(
        "<form action=\"{}\" method=\"post\" name=\"{fid}\"{tfclass} enctype=\"multipart/form-data\" id=\"{fid}\" ><input type='hidden' name='csrf_token' value='{}'/>",
        action, esc(&sc.inner.csrf), tfclass = if crate::db::g6() { "" } else { " class=\"trackerform\"" }
    );
    for k in ["backtolist", "backto", "backtoid", "backtransition"] {
        if has(p, k) && !(k != "backtolist" && csv(&s(&tr.row, "editfields")).iter().any(|n| t.field(n).map(|f| s(f, "field_type") == "BelongsTo" && has(p, n)).unwrap_or(false))) {
            o.push_str(&format!("<input type=\"hidden\" name=\"{}\" value=\"{}\" id=\"{}\" />", k, esc(&s(p, k)), k));
        }
    }
    if let Some(rid) = id {
        o.push_str(&format!("<input type=\"hidden\" name=\"id\" value=\"{}\" id=\"id\" />", rid));
    }
    let mut shown = 0;
    let mut z = 100;
    for n in csv(&s(&tr.row, "editfields")) {
        let Some(f) = t.field(&n) else { continue };
        let html = field_html(sc, t, tr, f, &datas, id, &required, z).await;
        z += 100;
        if html.contains("fieldcontain") {
            shown += 1;
        }
        o.push_str(&html);
    }
    if let Some(ns) = tr.next {
        o.push_str(&format!("<input type=\"hidden\" name=\"next_status\" value=\"{}\" id=\"next_status\" /><input type=\"hidden\" name=\"transition_{}\" value=\"{}\" id=\"transition_{}\" />", ns, tr.id, ns, tr.id));
    }
    let submit = { let x = s(&tr.row, "submitbuttontext"); if !x.is_empty() { x } else if id.is_some() { "Update".to_string() } else { "Create".to_string() } };
    let confirm = if tr.name.to_lowercase() == "delete" { " onclick=\"return confirm('Are you sure?')\"" } else { "" };
    o.push_str(&format!("<div class='fieldcontain submitbuttons row'><input type=\"submit\" name=\"submit\" class=\"save button btn btn-primary mx-auto col-1\" value=\"{}\" id=\"submit\"{} />", esc(&submit), confirm));
    if !opts.contains_key("nocancel") && b(&tr.row, "cancelbutton") {
        let back = match id {
            Some(rid) => format!("{}/{}", t.url("display"), rid),
            None => t.url("list"),
        };
        let ct = { let x = s(&tr.row, "cancelbuttontext"); if x.is_empty() { "Cancel".to_string() } else { x } };
        o.push_str(&format!("<a href=\"{}\" name=\"cancel\" class=\"save button btn btn-warning mx-auto col-2\">{}</a>", back, esc(&ct)));
    }
    o.push_str("</div>");
    if b(&tr.row, "immediate_submission") && shown == 0 && !p.contains_key("submit") {
        o.push_str(&format!("<script>$(window).on('load',function() {{ $('#{fid}').find('input[type=\"submit\"]').click();}});</script>"));
    }
    o.push_str("</form>");
    o.push_str(FORM_JS);
    o.push_str("<script>setTimeout(update_submit, 300);</script>");
    o
}

// ---------------- display ----------------

pub async fn can_view(sc: &Sc, t: &Tracker, datas: &Row) -> bool {
    if b(&t.row, "anonymous_view") {
        return true;
    }
    if sc.inner.user.is_none() {
        return false;
    }
    if !t.module_roles(sc).await.is_empty() {
        return true;
    }
    !t.user_roles(sc, Some(datas)).await.is_empty()
}

/// Transition buttons for a record, sorted by name as in the Grails portal.
pub async fn transition_buttons(sc: &Sc, t: &Tracker, id: i64) -> String {
    let Some(datas) = t.get(sc.db(), id).await else { return String::new() };
    let mut trs = t.available_transitions(sc, &datas).await;
    trs.sort_by(|a, b| a.name.cmp(&b.name));
    let mut o = String::new();
    for tr in trs {
        o.push_str(&format!(
            "<a href=\"/{}/{}/{}/{}\" class=\"btn btn-primary m-1 {}\" name=\"transition_{}\">{}</a>",
            t.module, t.slug, tr.url_name(), id, esc(&s(&tr.row, "button_class")), tr.id, esc(&tr.label())
        ));
    }
    o
}

/// The Grails portals' trackerTransitions tag: someone who may not see the record is sent
/// back to the list; otherwise the record's transition buttons, then a non-breaking space.
pub async fn tracker_transitions(sc: &Sc, t: &Tracker, id: i64) -> String {
    let Some(datas) = t.get(sc.db(), id).await else { return String::new() };
    if !can_view(sc, t, &datas).await {
        return format!("<script>alert('You are not authorised to view that record');window.location='{}';</script>", t.url("list"));
    }
    format!("{}&nbsp;", transition_buttons(sc, t, id).await)
}

/// The records of another tracker that belong to this one (a HasMany field).
async fn has_many(sc: &Sc, t: &Tracker, f: &Row, id: i64, form: Option<&Trans>) -> String {
    let Some(ot) = t.other_tracker(sc.db(), f).await else { return String::new() };
    let Some(link) = ot.fields.iter().find(|x| s(x, "field_type") == "BelongsTo" && s(x, "field_options").split(':').any(|p| p.trim() == t.slug)) else { return String::new() };
    let lname = s(link, "name");
    let tokens: Vec<String> = { let l = csv(&s(f, "field_format")); if l.is_empty() { csv(&s(&ot.row, "listfields")) } else { l } };
    let numbered = tokens.iter().any(|x| x == "#") || form.is_some();
    let cols: Vec<&Row> = tokens.iter().filter_map(|n| ot.field(n)).filter(|c| s(c, "field_type") != "HasMany").collect();
    // g5 shows View only when field_format lists it; g6 always does
    let view = crate::db::g6() || tokens.iter().any(|x| x.eq_ignore_ascii_case("view"));
    let acts: Vec<&Trans> = ot.transitions.iter().filter(|tr| tokens.iter().any(|x| x.eq_ignore_ascii_case(&tr.name))).collect();
    let order = { let d = s(&ot.row, "defaultsort"); if d.is_empty() { "id".to_string() } else { d } };
    let rows = ot.rows(sc.db(), Some(&row! {lname.clone()=>id}), Some(&order), None, Some(1000)).await.unwrap_or_default();
    let mut o = String::new();
    if form.is_some() {
        o.push_str("<div style='margin: 2px 0px 2px 10px; padding: 0px;'>");
    }
    o.push_str(&format!("<table id='field_{}'>", esc(&s(f, "name"))));
    if let Some(tr) = form {
        o.push_str(&format!("<tr><td><a href=\"/{}/{}/create?{}={}&amp;backtransition={}\" class=\"btn btn-primary\">Add</a></td></tr>", ot.module, ot.slug, enc(&lname), id, tr.id));
    }
    o.push_str("<tr>");
    if numbered {
        o.push_str("<th>#</th>");
    }
    for c in &cols {
        let l = s(c, "label");
        o.push_str(&format!("<th id='col_{}'>{}</th>", esc(&l.replace(' ', "_").to_lowercase()), esc(&l)));
    }
    o.push_str(if form.is_some() { if acts.is_empty() { "" } else { "<th>Action</th>" } } else { "<th>Actions</th>" });
    o.push_str("</tr>");
    for (k, r) in rows.iter().enumerate() {
        let rid = i(r, "id").unwrap_or(0);
        o.push_str(&format!("<tr class='{}'>", esc(&s(r, "record_status").replace(' ', "_").to_lowercase())));
        if numbered {
            o.push_str(&format!("<td>{}</td>", k + 1));
        }
        for c in &cols {
            let l = s(c, "label");
            o.push_str(&format!("<td id='col_{}'>{}</td>", esc(&l.replace(' ', "_").to_lowercase()), value_div(sc, &ot, c, r.get(&s(c, "name")).unwrap_or(&Value::Null), r, false).await));
        }
        if form.is_none() || !acts.is_empty() {
            o.push_str("<td>");
            if view {
                o.push_str(&format!("<a href=\"/{}/{}/display/{}?{}={}\" class=\"btn btn-secondary hasmanyview\">View</a>", ot.module, ot.slug, rid, enc(&lname), id));
            }
            let backto = format!("{}:{}", t.module, t.slug);
            for tr in &acts {
                if !ot.matches_prev(tr, &s(r, "record_status")) || !ot.transition_allowed(sc, &tr.name, Some(r)).await {
                    continue;
                }
                if tr.name.eq_ignore_ascii_case("delete") {
                    o.push_str(&format!("<a href=\"/{}/{}/delete/{}?{}={}&amp;backto={}&amp;backtoid={}\" class=\"btn btn-danger hasmanydelete\" onclick=\"return confirm('Confirm delete record?');\">Delete</a>", ot.module, ot.slug, rid, enc(&lname), id, enc(&backto), id));
                } else {
                    let txt = { let x = s(&tr.row, "submitbuttontext"); if x.is_empty() { tr.name.clone() } else { x } };
                    o.push_str(&format!("<a href=\"/{}/{}/{}/{}?{}={}&amp;backto={}\" class=\"btn btn-secondary hasmanyaction\">{}</a>", ot.module, ot.slug, tr.url_name(), rid, enc(&lname), id, enc(&backto), esc(&txt)));
                }
            }
            o.push_str("</td>");
        }
        o.push_str("</tr>");
    }
    o.push_str("</table>");
    if form.is_some() {
        o.push_str("</div>");
    }
    o
}

/// One row of a record view (the Grails portal's trackerFieldRow).
fn field_row<'a>(sc: &'a Sc, t: &'a Tracker, f: &'a Row, datas: &'a Row, id: i64) -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send + 'a>> {
    Box::pin(async move {
        let name = s(f, "name").trim().to_string();
        let ft = s(f, "field_type");
        let label = s(f, "label");
        if ft == "Hidden" {
            return String::new();
        }
        let tip = if has(f, "field_tooltip") { format!(" title='{}'", esc(&s(f, "field_tooltip"))) } else { String::new() };
        let mut o = format!("<tr class='prop' id='{}_tr'>", esc(&name));
        if ft == "FieldGroup" {
            o.push_str(&format!(
                "<tbody class='section-group'><tr><th class='group_heading' colspan='2'>{}{}</th></tr><tr><td class='group_data' colspan='2'><table>",
                esc(&label), if has(f, "field_tooltip") { format!(" <span class='field-tooltip'{}>&#9432;</span>", tip) } else { String::new() }
            ));
            for m in csv(&s(f, "field_options")) {
                if let Some(mf) = t.field(&m) {
                    o.push_str(&field_row(sc, t, mf, datas, id).await);
                }
            }
            o.push_str("</table></td></tr></tbody></tr>");
            return o;
        }
        let colspan = if b(f, "hide_heading") { " colspan='2' style='text-align:center'" } else { "" };
        if colspan.is_empty() {
            o.push_str(&format!("<td class='name'><span{}>{}</span></td>", tip, esc(&label)));
        }
        let val = if ft == "HasMany" {
            format!("<div id='{}_value_div'>{}</div>", esc(&name), has_many(sc, t, f, id, None).await)
        } else {
            value_div(sc, t, f, datas.get(&name).unwrap_or(&Value::Null), datas, false).await
        };
        o.push_str(&format!("<td class='value val_{}'{}>{}</td></tr>", esc(&label.replace(' ', "_").to_lowercase()), colspan, val));
        o
    })
}

/// Fields shown for a record: its status's displayfields, else the list fields.
fn display_names(t: &Tracker, datas: &Row) -> Vec<String> {
    let st = if t.tracker_type == "DataStore" { t.initial_status().and_then(|x| t.status_by_id(x)) } else { t.status(&s(datas, "record_status")) };
    let mut names: Vec<String> = st.map(|x| csv(&s(x, "displayfields"))).unwrap_or_default().into_iter().filter(|n| t.field(n).is_some()).collect();
    if names.is_empty() {
        names = csv(&s(&t.row, "listfields")).into_iter().filter(|n| t.field(n).is_some()).collect();
    }
    if names.is_empty() {
        names = t.fields.iter().map(|f| s(f, "name")).collect();
    }
    names
}

pub async fn tracker_display(sc: &Sc, t: &Tracker, id: i64, opts: &Row) -> String {
    let Some(datas) = t.get(sc.db(), id).await else { return String::new() };
    if !can_view(sc, t, &datas).await {
        return format!("<script>alert('You are not authorised to view that record');window.location='{}';</script>", t.url("list"));
    }
    let names = match opts.get("fields") {
        Some(Value::String(f)) => csv(f),
        _ => display_names(t, &datas),
    };
    let mut o = String::from("<table class='requestview'>");
    for n in names {
        if let Some(f) = t.field(&n) {
            o.push_str(&field_row(sc, t, f, &datas, id).await);
        }
    }
    o.push_str("</table>");
    // the "Update Status" form, when the record's status allows updates by this user
    o.push_str(&format!("<form action=\"/{}/{}/updaterecord/{}\" method=\"post\" name=\"updaterecord\" enctype=\"multipart/form-data\" id=\"updaterecord\" ><input type='hidden' name='csrf_token' value='{}'/>", t.module, t.slug, id, esc(&sc.inner.csrf)));
    if let Some(st) = t.status(&s(&datas, "record_status")) {
        let uroles = t.user_roles(sc, Some(&datas)).await;
        let rules = s(st, "updateallowedroles");
        let allowed = if rules.trim().is_empty() {
            sc.inner.user.is_some()
        } else {
            let who: Vec<String> = rules.split(';').flat_map(|r| csv(r.split("->").next().unwrap_or(""))).collect();
            who.iter().any(|w| uroles.contains(w))
        };
        if b(st, "updateable") && allowed && t.tracker_type == "Tracker" {
            o.push_str(&format!(
                "<h2>Update Status</h2><input type=\"hidden\" name=\"id\" value=\"{id}\" id=\"id\" /><input type=\"hidden\" name=\"statusUpdate\" value=\"1\" id=\"statusUpdate\" /><input type=\"hidden\" name=\"record_status\" value=\"{}\" id=\"record_status\" /><label>Remarks/Description</label><textarea name=\"statusUpdateDesc\" style=\"float:none;clear:both;width:100%;\" cols=\"150\" id=\"statusUpdateDesc\" ></textarea>",
                esc(&s(&datas, "record_status"))
            ));
            if b(st, "attachable") {
                o.push_str("<label>Attach File:</label><input style='float:none;' type='file' name='uploadfile'/><br/>");
            }
            if !b(st, "suppressupdatebutton") {
                o.push_str("<input type=\"submit\" name=\"submit\" class=\"btn btn-primary m-1\" style=\"float:none;clear:right;\" value=\"Submit Update\" id=\"submit\" />");
            }
        }
    }
    o.push_str("</form>");
    if !opts.contains_key("nobuttons") {
        o.push_str(&transition_buttons(sc, t, id).await);
    }
    o.push_str("&nbsp;\n            <script>\n            function toggleAll(field_name,value=null) {\n                var dispelement = $(\"#\" + field_name + \"_tr\");\n                if(dispelement) {\n                    if(value!=null) { if(value==1) { dispelement.show(); } else { dispelement.hide(); } }\n                    else { dispelement.toggle(); }\n                }\n            }\n            function update_submit(){}\n            </script>");
    o
}

/// The record's status updates (audit trail).
pub async fn tracker_updates(sc: &Sc, t: &Tracker, id: i64) -> String {
    if t.tracker_type != "Tracker" {
        return String::new();
    }
    let db = sc.db();
    let Some(datas) = t.get(db, id).await else { return String::new() };
    if !can_view(sc, t, &datas).await {
        return String::new();
    }
    let uroles = t.user_roles(sc, Some(&datas)).await;
    let trail = db
        .rows(
            &format!("select tr.*, u.name as updater_name from {} tr left join portal_user u on u.id=tr.updater_id where tr.record_id=:id order by tr.update_date desc, tr.id desc", qcol(&t.trail_table())),
            &row! {"id"=>id},
        )
        .await
        .unwrap_or_default();
    let visible: Vec<&Row> = trail
        .iter()
        .filter(|r| {
            let vis: Vec<String> = csv(&s(r, "allowedroles")).into_iter().filter(|x| x != "null").collect();
            vis.is_empty() || vis.iter().any(|v| uroles.contains(v))
        })
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut o = String::from("<h3>Status Updates</h3><table style=background-color:white id='statusupdates'><tr><th>Update Description</th><th>Updated By</th><th>Update Date</th></tr>");
    for r in visible {
        o.push_str(&format!("<tr><td><p>{}</p>", esc(&s(r, "description")).replace('\n', "<br/>")));
        if let Some(a) = i(r, "attachment_id") {
            if let Ok(Some(fl)) = db.get("file_link", a).await {
                o.push_str(&format!("Attached file : <a href=\"/fileLink/get/{}\">{}</a><br/>", a, esc(&s(&fl, "name"))));
            }
        }
        o.push_str(&format!("</td><td>{}</td><td>{}</td></tr>", esc(&s(r, "updater_name")), esc(&crate::groovy::java_date(&s(r, "update_date"), "HH:mm a dd-MMM-yy"))));
    }
    o.push_str("</table>");
    o
}

/// The text of a Groovy string literal (triple or single quoted, either quote);
/// other text is returned as it is.
pub fn groovy_literal(code: &str) -> String {
    let c = code.trim();
    for q in ["\x27\x27\x27", "\"\"\"", "\x27", "\""] {
        if c.len() >= 2 * q.len() && c.starts_with(q) && c.ends_with(q) {
            return c[q.len()..c.len() - q.len()].to_string();
        }
    }
    c.to_string()
}
