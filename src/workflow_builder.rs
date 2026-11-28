//! Interactive visual Workflow Graph & Builder matching g6ikram's
//! `views/portalTracker/workflow_graph.gsp`, `tracker_builder.js`, and
//! `PortalTrackerController.builder_*` JSON endpoints.

use crate::db::{self, b, csv, i, s, vs, Db, Row};
use crate::entity::FIELD_TYPES;
use crate::row;
use crate::tracker::Tracker;
use crate::web::{self, esc, now_str, Ctx};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const TRACKER_FIELD_LISTS: &[&str] = &["listfields", "hiddenlistfields", "excelfields", "searchfields", "filterfields"];
const ROLE_TYPES: &[&str] = &["User Role", "Data Compare"];

/// Safe JSON for embedding inside a `<script>` tag.
fn script_json(v: &Value) -> String {
    v.to_string()
        .replace("</", "<\\/")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

async fn can_build(ctx: &Ctx, t: &Tracker) -> bool {
    let actor = match i(&ctx.sess, "realuserid") {
        Some(ruid) => match web::load_user(ctx.db(), ruid).await {
            Some(ru) => Some(ru),
            None => ctx.user.clone(),
        },
        None => ctx.user.clone(),
    };
    let Some(u) = actor else { return false };
    if b(&u, "is_admin") {
        return true;
    }
    web::developer_list(ctx.db(), &u).await.iter().any(|m| m == &t.module)
}

fn ordered_fields(t: &Tracker) -> Vec<Row> {
    let mut fs = t.fields.clone();
    fs.sort_by(|a, b| {
        let oa = i(a, "field_order").unwrap_or(i64::MAX);
        let ob = i(b, "field_order").unwrap_or(i64::MAX);
        oa.cmp(&ob).then_with(|| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()))
    });
    fs
}

fn filter_in_order(canonical: &[String], wanted: &[String]) -> String {
    let set: HashSet<&str> = wanted.iter().map(|x| x.trim()).filter(|x| !x.is_empty()).collect();
    canonical.iter().filter(|n| set.contains(n.as_str())).cloned().collect::<Vec<_>>().join(",")
}

async fn builder_model(db: &Db, t: &Tracker) -> Value {
    let mut lists = serde_json::Map::new();
    for &prop in TRACKER_FIELD_LISTS {
        lists.insert(prop.into(), json!(csv(&s(&t.row, prop))));
    }

    let fields: Vec<Value> = ordered_fields(t)
        .into_iter()
        .map(|f| {
            let ft = s(&f, "field_type");
            let mut m = json!({
                "id": i(&f, "id").unwrap_or(0).to_string(),
                "name": s(&f, "name"),
                "label": s(&f, "label"),
                "field_type": ft
            });
            if ft == "FieldGroup" {
                m["members"] = json!(csv(&s(&f, "field_options")));
            }
            m
        })
        .collect();

    let mut roles_sorted = t.roles.clone();
    roles_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));
    let mut role_id_by_name: HashMap<String, String> = HashMap::new();
    let roles: Vec<Value> = roles_sorted
        .iter()
        .map(|r| {
            let rid = i(r, "id").unwrap_or(0).to_string();
            let nm = s(r, "name");
            role_id_by_name.insert(nm.clone(), rid.clone());
            json!({"id": rid, "name": nm, "role_type": s(r, "role_type")})
        })
        .collect();

    let pages: Vec<Value> = db
        .rows(&format!("select id, slug, title, runable from \"{}\" where \"module\"=:m order by slug", db::page_t()), &row! {"m"=>t.module.clone()})
        .await
        .unwrap_or_default()
        .iter()
        .map(|p| {
            json!({
                "id": i(p, "id").unwrap_or(0).to_string(),
                "slug": s(p, "slug"),
                "title": s(p, "title"),
                "runable": b(p, "runable")
            })
        })
        .collect();

    let mut statuses_sorted = t.statuses.clone();
    statuses_sorted.sort_by(|a, b| s(a, "name").to_lowercase().cmp(&s(b, "name").to_lowercase()));
    let statuses: Vec<Value> = statuses_sorted
        .iter()
        .map(|st| {
            json!({
                "id": i(st, "id").unwrap_or(0).to_string(),
                "name": s(st, "name"),
                "updateable": b(st, "updateable"),
                "attachable": b(st, "attachable"),
                "displayfields": csv(&s(st, "displayfields")),
                "editfields": csv(&s(st, "editfields")),
                "editroles": csv(&s(st, "editroles")),
                "updateallowedroles": csv(&s(st, "updateallowedroles"))
            })
        })
        .collect();

    // Load transition -> role_ids directly from the join table so IDs match exact DB rows
    let mut tr_role_ids: HashMap<i64, Vec<String>> = HashMap::new();
    if let Ok(jrows) = db
        .rows(
            "select portal_tracker_transition_roles_id as tid, portal_tracker_role_id as rid \
             from portal_tracker_transition_portal_tracker_role \
             where portal_tracker_transition_roles_id in (select id from portal_tracker_transition where tracker_id=:t)",
            &row! {"t"=>t.id},
        )
        .await
    {
        for jr in jrows {
            if let (Some(tid), Some(rid)) = (i(&jr, "tid"), i(&jr, "rid")) {
                tr_role_ids.entry(tid).or_default().push(rid.to_string());
            }
        }
    }

    let mut trans_sorted = t.transitions.clone();
    trans_sorted.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let transitions: Vec<Value> = trans_sorted
        .iter()
        .map(|tr| {
            let rids = tr_role_ids.get(&tr.id).cloned().unwrap_or_else(|| {
                tr.roles.iter().filter_map(|rn| role_id_by_name.get(rn).cloned()).collect()
            });
            json!({
                "id": tr.id.to_string(),
                "name": tr.name,
                "display_name": s(&tr.row, "display_name"),
                "same_status": b(&tr.row, "same_status"),
                "next_status_id": tr.next.map(|x| x.to_string()),
                "postprocess_id": i(&tr.row, "postprocess_id").map(|x| x.to_string()),
                "prev_status_ids": tr.prev.iter().map(|x| x.to_string()).collect::<Vec<_>>(),
                "role_ids": rids,
                "displayfields": csv(&s(&tr.row, "displayfields")),
                "editfields": csv(&s(&tr.row, "editfields"))
            })
        })
        .collect();

    json!({
        "tracker": {
            "id": t.id,
            "name": t.name,
            "module": t.module,
            "slug": t.slug,
            "initial_status_id": t.initial_status().map(|x| x.to_string())
        },
        "lists": lists,
        "fields": fields,
        "roles": roles,
        "pages": pages,
        "statuses": statuses,
        "transitions": transitions
    })
}

fn json_err(code: StatusCode, msg: &str) -> Response {
    (
        code,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        json!({"error": msg}).to_string(),
    )
        .into_response()
}

fn parse_payload(ctx: &Ctx) -> serde_json::Map<String, Value> {
    if !ctx.body.is_empty() {
        if let Ok(Value::Object(m)) = serde_json::from_slice::<Value>(&ctx.body) {
            return m;
        }
    }
    ctx.params.clone()
}

fn str_vec(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::Array(arr)) => arr.iter().map(vs).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => vec![],
    }
}

fn opt_id_val(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if v.is_null() {
        return None;
    }
    v.as_i64().or_else(|| vs(v).trim().parse::<i64>().ok())
}

// ---------------- GET /portalTracker/workflow_graph/{id} ----------------

pub async fn workflow_graph(ctx: Ctx, t: &Tracker) -> Response {
    let db = &ctx.st.db.clone();
    let can_edit = can_build(&ctx, t).await;
    let init_id = t.initial_status();
    let init_name = init_id.map(|id| t.status_name(Some(id))).filter(|x| !x.is_empty()).unwrap_or_else(|| "Not set".into());

    let nodes: Vec<Value> = t
        .statuses
        .iter()
        .map(|st| {
            json!({
                "id": i(st, "id").unwrap_or(0).to_string(),
                "label": s(st, "name"),
                "updateable": b(st, "updateable"),
                "attachable": b(st, "attachable"),
                "flow": i(st, "flow")
            })
        })
        .collect();

    let mut edges: Vec<Value> = vec![];
    for tr in &t.transitions {
        let same = b(&tr.row, "same_status");
        let next_s = tr.next.map(|x| x.to_string());
        let roles_s = tr.roles.join(", ");
        let disp = tr.label();
        if !tr.prev.is_empty() {
            for &pid in &tr.prev {
                edges.push(json!({
                    "id": format!("{}_{}", tr.id, pid),
                    "from": pid.to_string(),
                    "to": if same { Some(pid.to_string()) } else { next_s.clone() },
                    "label": tr.name,
                    "roles": roles_s,
                    "displayName": disp,
                    "sameStatus": same
                }));
            }
        } else {
            edges.push(json!({
                "id": tr.id.to_string(),
                "from": Value::Null,
                "to": if same { init_id.map(|x| x.to_string()) } else { next_s },
                "label": tr.name,
                "roles": roles_s,
                "displayName": disp,
                "sameStatus": same,
                "isNew": true
            }));
        }
    }

    let builder_json = if can_edit { script_json(&builder_model(db, t).await) } else { "null".into() };
    let nodes_json = script_json(&Value::Array(nodes.clone()));
    let edges_json = script_json(&Value::Array(edges.clone()));
    let field_types_json = script_json(&json!(FIELD_TYPES));
    let role_types_json = script_json(&json!(ROLE_TYPES));

    let builder_script_tag = if can_edit {
        "<script src=\"/static/js/tracker_builder.js\"></script>"
    } else {
        ""
    };
    let builder_add_btns = if can_edit {
        "<button id=\"tb-add-status\">+ Status</button><button id=\"tb-add-transition\">+ Transition</button>"
    } else {
        ""
    };
    let builder_panel_html = if can_edit {
        r##"<div id="builder-panel">
  <div id="builder-selection"></div>
  <div id="builder-message" class="builder-message"></div>
  <div id="builder-props"></div>

  <div class="tb-tables">
    <div class="tb-table" id="builder-fields-table">
      <h5>Fields
        <span class="tb-hint" style="display:inline">drag a row to set the tracker's field order</span>
        <button type="button" id="tb-toggle-cols" class="tb-linkish">show tracker columns</button>
        <button type="button" id="tb-infer-order" class="tb-linkish"
          title="Work out an order from the field lists already stored on every status and transition">suggest order from forms</button>
      </h5>
      <div class="tb-head">
        <span></span><span>Name</span><span>Label</span><span>Type</span>
        <span class="tb-check">View</span><span class="tb-check">Edit</span>
        <span class="tb-check tb-xtra" title="Columns shown on the tracker's list page">List</span>
        <span class="tb-check tb-xtra" title="Fetched with each row but not displayed - available to row classes and links">Hidden</span>
        <span class="tb-check tb-xtra" title="Columns included in the Excel download">Excel</span>
        <span class="tb-check tb-xtra" title="Fields the list page's search box looks in">Search</span>
        <span class="tb-check tb-xtra" title="Fields offered as filters on the list page">Filter</span>
      </div>
      <div class="tb-rows" id="builder-field-rows"></div>
      <div class="tb-hint">A <strong>Field Group</strong> row expands (&#9654;) to show what it contains.
        Ticking the group renders every field inside it as one fieldset, so its members show a
        &middot; rather than needing their own tick.</div>
      <div class="tb-hint tb-xtra">These five are tracker-wide, not per status or transition - they save with the same Save button.</div>
    </div>
    <div class="tb-table" id="builder-roles-table">
      <h5><span id="builder-roles-title">Roles</span>
        <button type="button" class="tb-linkish tb-sidetoggle" data-panel="builder-roles-table">hide</button></h5>
      <div class="tb-head tb-rolehead"><span>Role</span><span>Type</span><span class="tb-check">On</span></div>
      <div class="tb-rows" id="builder-role-rows"></div>
    </div>
    <div class="tb-table" id="builder-prev-table" style="display:none">
      <h5>Comes from <span class="tb-hint" style="display:inline">which statuses this transition starts from</span>
        <button type="button" class="tb-linkish tb-sidetoggle" data-panel="builder-prev-table">hide</button></h5>
      <div class="tb-head tb-prevhead"><span>Status</span><span class="tb-check">On</span></div>
      <div class="tb-rows" id="builder-prev-rows"></div>
      <div class="tb-hint" id="builder-prev-note"></div>
    </div>
  </div>

  <div class="tb-actions">
    <button id="tb-save">Save</button>
    <button id="tb-revert">Revert</button>
    <button id="tb-add-self" style="display:none">+ Self-transition</button>
    <button id="tb-delete" class="tb-danger" style="display:none">Delete</button>
    <button id="tb-toggle-fields">Add Fields</button>
    <button id="tb-toggle-roles">Add Roles</button>
  </div>

  <div class="tb-inline" id="tb-groupbox" style="display:none"></div>

  <div class="tb-inline" id="tb-addfields-box" style="display:none">
    <strong>Add fields</strong>
    <div class="tb-hint">Columns are created in the database as soon as you add them.</div>
    <div id="tb-newfields"></div>
    <button type="button" id="tb-add-field-row">+ Row</button>
    <button type="button" id="tb-create-fields">Create Fields</button>
  </div>

  <div class="tb-inline" id="tb-addroles-box" style="display:none">
    <strong>Add roles</strong>
    <div class="tb-hint">A <em>User Role</em> gets its members from the module's roles and needs no rule.
      A <em>Data Compare</em> role matches a record against the rule - an expression rendering a SQL
      condition, e.g. <code>reviewed_by_staff='${curuser?.id}'</code>. Leave the rule blank to fill it in later.</div>
    <div id="tb-newroles"></div>
    <button type="button" id="tb-add-role-row">+ Row</button>
    <button type="button" id="tb-create-roles">Create Roles</button>
  </div>

  <div class="tb-inline" id="tb-add-status-box" style="display:none">
    <strong>New status</strong>
    <input type="text" id="tb-new-status-name" placeholder="Status name">
    <button type="button" id="tb-create-status">Create</button>
    <button type="button" id="tb-cancel-status">Cancel</button>
    <div class="tb-hint">You can also double-click an empty spot on the canvas.</div>
  </div>

  <div class="tb-inline" id="tb-add-transition-box" style="display:none">
    <strong>New transition</strong> <span id="tb-new-transition-where"></span>
    <input type="text" id="tb-new-transition-name" placeholder="Transition name">
    <button type="button" id="tb-create-transition">Create</button>
    <button type="button" id="tb-cancel-transition">Cancel</button>
  </div>
</div>"##
    } else {
        ""
    };

    let builder_cfg_js = if can_edit {
        format!(
            "{{
                trackerId: {tid},
                model: {builder_json},
                fieldTypes: {field_types_json},
                roleTypes: {role_types_json},
                urls: {{
                    addFields:        '/portalTracker/builder_add_fields',
                    addRoles:         '/portalTracker/builder_add_roles',
                    saveFieldOrder:   '/portalTracker/builder_save_field_order',
                    saveTrackerLists: '/portalTracker/builder_save_tracker_lists',
                    saveStatus:       '/portalTracker/builder_save_status',
                    deleteStatus:     '/portalTracker/builder_delete_status',
                    saveTransition:   '/portalTracker/builder_save_transition',
                    deleteTransition: '/portalTracker/builder_delete_transition',
                    saveFieldGroup:   '/portalTracker/builder_save_field_group'
                }}
            }}",
            tid = t.id
        )
    } else {
        "null".into()
    };

    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"alert alert-info\" role=\"status\">{}</div>", esc(&flash)) };
    let init_id_str = init_id.map(|x| x.to_string()).unwrap_or_default();
    let ttype = if t.tracker_type.is_empty() { "Not specified" } else { &t.tracker_type };

    let body = format!(
        r##"<script src="/static/libs/vis-network/vis-network.min.js"></script>
{builder_script_tag}
<style>
#workflow-network {{ width: 100%; height: 700px; border: 1px solid lightgray; background-color: #fafafa; }}
.legend {{ background: white; border: 1px solid #ccc; padding: 15px; margin-top: 20px; border-radius: 5px; }}
.legend-item {{ display: flex; align-items: center; margin-bottom: 10px; }}
.legend-icon {{ width: 30px; height: 30px; margin-right: 10px; border-radius: 50%; display: inline-block; }}
.status-node {{ background-color: #97C2FC; border: 2px solid #2B7CE9; }}
.initial-status {{ background-color: #7BE141; border: 2px solid #41A906; }}
.updateable-status {{ background-color: #FFA807; border: 2px solid #FA8E06; }}
.controls {{ margin: 20px 0; display: flex; gap: 10px; flex-wrap: wrap; }}
.controls button {{ padding: 8px 16px; border-radius: 4px; border: 1px solid #ccc; background: white; cursor: pointer; }}
.controls button:hover {{ background: #f0f0f0; }}
#node-info {{ background: white; border: 1px solid #ccc; padding: 15px; margin-top: 20px; border-radius: 5px; display: none; }}
#node-info.active {{ display: block; }}
#builder-panel {{ border: 1px solid #ccc; border-radius: 5px; background: #fff; padding: 15px; margin-bottom: 20px; }}
#builder-selection {{ margin-bottom: 10px; }}
.tb-hint {{ color: #777; font-size: 12px; display: block; margin-top: 3px; }}
.tb-dirty {{ background: #FFA807; color: #fff; border-radius: 3px; padding: 1px 7px; font-size: 11px; margin-left: 6px; }}
.builder-message {{ display: none; padding: 8px 12px; border-radius: 4px; margin: 10px 0; font-size: 13px; }}
.builder-message.show {{ display: block; }}
.builder-message.ok {{ background: #e6f7e6; border: 1px solid #7BE141; }}
.builder-message.warn {{ background: #fff6e0; border: 1px solid #FFA807; }}
.builder-message.error {{ background: #fdecea; border: 1px solid #e57373; }}
.builder-message.info {{ background: #eef4fd; border: 1px solid #97C2FC; }}
.tb-props {{ display: flex; flex-wrap: wrap; gap: 15px; align-items: center; margin-bottom: 12px; }}
.tb-props label {{ display: flex; align-items: center; gap: 6px; font-size: 13px; margin: 0; }}
.tb-props input[type=text], .tb-props select {{ padding: 3px 6px; }}
.tb-props .tb-off {{ opacity: 0.4; }}
.tb-tables {{ display: flex; gap: 20px; flex-wrap: wrap; align-items: flex-start; }}
.tb-table {{ flex: 1 1 300px; min-width: 320px; }}
.tb-table h5 {{ margin: 0 0 6px; font-size: 13px; }}
.tb-head, .tb-row, .tb-subrow {{ display: grid; grid-template-columns: 22px 1.4fr 1.4fr 1fr 46px 46px; align-items: center; gap: 6px; padding: 4px 6px; font-size: 12px; }}
.tb-rolerow, .tb-rolehead {{ grid-template-columns: 1.4fr 1fr 46px; }}
.tb-prevrow, .tb-prevhead {{ grid-template-columns: 1.4fr 46px; }}
.tb-xtra {{ display: none; }}
#builder-fields-table.tb-expanded {{ flex-basis: 100%; }}
#builder-fields-table.tb-expanded .tb-xtra {{ display: block; }}
#builder-fields-table.tb-expanded .tb-head,
#builder-fields-table.tb-expanded .tb-row,
#builder-fields-table.tb-expanded .tb-subrow {{ grid-template-columns: 22px 1.4fr 1.4fr 1fr 46px 46px 46px 56px 46px 56px 46px; }}
#builder-fields-table.tb-expanded .tb-hint.tb-xtra {{ display: block; margin-top: 6px; }}
.tb-linkish {{ border: none !important; background: none !important; color: #2B7CE9; cursor: pointer; font-size: 12px; padding: 0 !important; text-decoration: underline; }}
.tb-head {{ font-weight: bold; border-bottom: 2px solid #ddd; color: #555; }}
.tb-row {{ border-bottom: 1px solid #f0f0f0; background: #fff; }}
.tb-row:hover {{ background: #f7fbff; }}
.tb-rows {{ max-height: 320px; overflow-y: auto; border: 1px solid #eee; }}
.tb-grip {{ cursor: grab; color: #bbb; letter-spacing: -3px; user-select: none; }}
.tb-name {{ font-family: monospace; }}
.tb-type {{ color: #888; }}
.tb-check {{ text-align: center; }}
.tb-empty {{ padding: 12px; color: #888; font-size: 12px; }}
.dragsort--dragElem {{ opacity: 0.4; }}
.tb-subrow {{ background: #fbfcfe; border-bottom: 1px dotted #eef1f4; color: #666; }}
.tb-subrow .tb-name {{ font-size: 11px; }}
.tb-subrow .tb-type {{ font-size: 11px; font-style: italic; }}
.tb-caret {{ cursor: pointer; user-select: none; color: #2B7CE9; text-align: center; font-size: 10px; }}
.tb-tree {{ color: #c3c9d1; font-family: monospace; white-space: pre; }}
.tb-gbadge {{ font-size: 10px; color: #5a6472; background: #eef1f4; border-radius: 3px; padding: 0 4px; margin-left: 5px; white-space: nowrap; }}
.tb-gbadge.tb-gon {{ background: #e0efe6; color: #2c6b4f; }}
.tb-implied {{ background: #f4fbf7; }}
.tb-dot {{ color: #2c6b4f; font-weight: bold; }}
.tb-groupbtn {{ border: none !important; background: none !important; color: #2B7CE9; cursor: pointer; font-size: 11px; padding: 0 !important; text-decoration: underline; }}
.tb-memberbox {{ max-height: 260px; overflow-y: auto; border: 1px solid #ddd; background: #fff; padding: 6px; margin: 6px 0; }}
.tb-memberbox label {{ display: block; font-size: 12px; padding: 1px 0; }}
.tb-memberbox .tb-mtype {{ color: #888; font-size: 11px; }}
.tb-memberbox .tb-mdis {{ opacity: 0.45; }}
.tb-table.tb-collapsed {{ flex: 0 0 auto; min-width: 0; }}
.tb-table.tb-collapsed .tb-head,
.tb-table.tb-collapsed .tb-rows,
.tb-table.tb-collapsed .tb-hint {{ display: none; }}
.tb-table.tb-collapsed h5 {{ margin-bottom: 0; white-space: nowrap; opacity: 0.7; }}
.tb-sidetoggle {{ margin-left: 6px; font-weight: normal; }}
.tb-actions {{ margin-top: 12px; display: flex; gap: 8px; flex-wrap: wrap; align-items: center; }}
.tb-actions button, .tb-inline button {{ padding: 6px 14px; border-radius: 4px; border: 1px solid #ccc; background: #fff; cursor: pointer; }}
.tb-actions button:disabled {{ opacity: 0.45; cursor: default; }}
.tb-danger {{ color: #c62828; border-color: #e57373 !important; }}
.tb-danger.tb-armed {{ background: #c62828 !important; color: #fff; }}
.tb-inline {{ margin-top: 12px; padding: 10px; background: #f7f7f7; border-radius: 4px; }}
.tb-inline input[type=text], .tb-inline select {{ padding: 4px 6px; }}
.tb-newfield, .tb-newrole {{ display: flex; gap: 8px; margin-bottom: 6px; align-items: center; }}
.tb-newfield input, .tb-newrole input {{ flex: 1 1 auto; }}
.tb-newrole .nr-rule {{ flex: 2 1 auto; font-family: monospace; font-size: 12px; }}
.nf-remove {{ border: none !important; background: none !important; color: #c62828; font-size: 18px; cursor: pointer; }}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTracker/index">Tracker List</a></li>
  <li><a class="show" href="/portalTracker/show/{tid}">Back to {tname}</a></li>
</ul></div></section>
<section class="row"><div class="col-12">
  <h1>Workflow Graph: {tname}</h1>
  <p class="text-muted">{tmod} - {tslug}</p>
  {flash_html}

  <div class="controls">
    <button onclick="network.fit();">Fit to Screen</button>
    <button onclick="network.moveTo({{scale: 1.0}});">Reset Zoom</button>
    <button onclick="relayout();">Re-apply Layout</button>
    {builder_add_btns}
    <button onclick="togglePhysics();">Toggle Physics</button>
    <button onclick="toggleSelfTransitions();">Toggle Self-Transitions</button>
    <button onclick="exportGraph();">Export as Image</button>
  </div>

  {builder_panel_html}

  <div id="workflow-network"></div>

  <div id="node-info">
    <h4 id="info-title">Node Information</h4>
    <div id="info-content"></div>
  </div>

  <div class="legend">
    <h4>Legend</h4>
    <div class="legend-item"><span class="legend-icon initial-status"></span><span>Initial Status (Entry Point)</span></div>
    <div class="legend-item"><span class="legend-icon updateable-status"></span><span>Updateable Status</span></div>
    <div class="legend-item"><span class="legend-icon status-node"></span><span>Regular Status</span></div>
    <div class="legend-item"><span style="display:inline-block;width:60px;height:3px;background:#848484;margin-right:10px;"></span><span>Transition (with role information)</span></div>
    <div class="legend-item"><span style="display:inline-block;width:60px;height:0;border-top:3px dashed #BBBBBB;margin-right:10px;"></span><span>Self-transition - stays in the same status (hidden by default, use Toggle Self-Transitions)</span></div>
  </div>

  <div class="mt-4">
    <h4>Tracker Statistics</h4>
    <ul>
      <li><strong>Total Statuses:</strong> {n_cnt}</li>
      <li><strong>Total Transitions:</strong> {e_cnt}</li>
      <li><strong>Initial Status:</strong> {init_name}</li>
      <li><strong>Tracker Type:</strong> {ttype}</li>
    </ul>
  </div>
</div></section></div></div>
<script type="text/javascript">
var nodesData = {nodes_json};
var edgesData = {edges_json};
var initialStatusId = "{init_id_str}";

var SELF_ANGLE_SLOTS = 4;
function spreadSelfLoops(edgeArray) {{
    var byNode = {{}};
    edgeArray.forEach(function(e) {{
        if (e.from !== e.to) {{ return; }}
        (byNode[e.from] = byNode[e.from] || []).push(e);
    }});
    Object.keys(byNode).forEach(function(nodeId) {{
        var group = byNode[nodeId];
        if (group.length < 2) {{ return; }}
        group.forEach(function(e, i) {{
            e.selfReference = {{
                size: 20 + i * 14,
                angle: (Math.PI / 4) + (i % SELF_ANGLE_SLOTS) * (Math.PI / 2),
                renderBehindTheNode: true
            }};
        }});
    }});
}}

window.spreadParallelEdges = function(edgeArray) {{
    spreadSelfLoops(edgeArray);
    var groups = {{}};
    edgeArray.forEach(function(e) {{
        if (e.from === e.to) {{ return; }}
        var key = [e.from, e.to].sort().join('\u0000');
        (groups[key] = groups[key] || []).push(e);
    }});
    Object.keys(groups).forEach(function(key) {{
        var group = groups[key];
        if (group.length < 2) {{ return; }}
        var canonicalFrom = key.split('\u0000')[0];
        group.forEach(function(e, i) {{
            var type = (i % 2 === 0) ? 'curvedCW' : 'curvedCCW';
            if (e.from !== canonicalFrom) {{
                type = (type === 'curvedCW') ? 'curvedCCW' : 'curvedCW';
            }}
            e.smooth = {{ enabled: true, type: type, roundness: 0.2 * (Math.floor(i / 2) + 1) }};
        }});
    }});
    return edgeArray;
}};

var builderEnabled = {can_edit_js};
var builderConfig = {builder_cfg_js};

var nodes, edges;
if (builderEnabled) {{
    var builderGraph = TrackerBuilder.graphData(builderConfig.model);
    nodes = new vis.DataSet(builderGraph.nodes);
    edges = new vis.DataSet(builderGraph.edges);
}} else {{
    nodes = new vis.DataSet(nodesData.map(function(node) {{
        var color = '#97C2FC', borderColor = '#2B7CE9', font = {{ size: 14 }};
        if (node.id === initialStatusId) {{ color = '#7BE141'; borderColor = '#41A906'; font.bold = true; }}
        else if (node.updateable) {{ color = '#FFA807'; borderColor = '#FA8E06'; }}
        return {{
            id: node.id, label: node.label,
            title: 'Status: ' + node.label + '<br>Updateable: ' + (node.updateable ? 'Yes' : 'No') + '<br>Attachable: ' + (node.attachable ? 'Yes' : 'No') + (node.flow ? '<br>Flow: ' + node.flow : ''),
            color: {{ background: color, border: borderColor, highlight: {{ background: color, border: '#000000' }} }},
            font: font, shape: 'box', margin: 10, data: node
        }};
    }}));
    edges = new vis.DataSet(window.spreadParallelEdges(edgesData.filter(function(edge) {{
        return edge.to !== null && edge.to !== undefined;
    }}).map(function(edge) {{
        var label = edge.displayName;
        if (edge.roles) {{ label += '\n[' + edge.roles + ']'; }}
        return {{
            id: edge.id, from: edge.from || 'start', to: edge.to, label: label,
            title: 'Transition: ' + edge.label + '<br>Roles: ' + (edge.roles || 'None') + (edge.sameStatus ? '<br>Stays in the same status' : ''),
            arrows: 'to', dashes: edge.sameStatus ? true : false,
            color: {{ color: edge.sameStatus ? '#BBBBBB' : '#848484', highlight: '#FF0000' }},
            font: {{ align: 'middle', size: 11, color: edge.sameStatus ? '#999999' : '#343434' }},
            smooth: {{ type: 'cubicBezier', roundness: 0.5 }},
            hidden: edge.sameStatus ? true : false, data: edge
        }};
    }})));
    if (edgesData.some(function(edge) {{ return edge.isNew; }})) {{
        nodes.add({{ id: 'start', label: 'START', color: {{ background: '#DDDDDD', border: '#888888' }}, shape: 'ellipse', font: {{ bold: true }} }});
    }}
}}

var container = document.getElementById('workflow-network');
var data = {{ nodes: nodes, edges: edges }};
function hierarchicalLayout(nodeSpacing, levelSeparation) {{
    return {{ enabled: true, direction: 'LR', sortMethod: 'directed', nodeSpacing: nodeSpacing, levelSeparation: levelSeparation, treeSpacing: 60 }};
}}
var options = {{
    layout: {{ hierarchical: hierarchicalLayout(100, 200) }},
    physics: {{ enabled: false }},
    interaction: {{ hover: true, tooltipDelay: 100, navigationButtons: true, keyboard: true }},
    nodes: {{ borderWidth: 2, borderWidthSelected: 3 }},
    edges: {{ width: 2, selectionWidth: 4 }}
}};
var network = new vis.Network(container, data, options);
var physicsEnabled = false;

network.on('click', function(params) {{
    var nodeInfo = document.getElementById('node-info');
    var infoContent = document.getElementById('info-content');
    var infoTitle = document.getElementById('info-title');
    if (params.nodes.length > 0) {{
        var nodeId = params.nodes[0], node = nodes.get(nodeId);
        if (node && node.data) {{
            infoTitle.textContent = 'Status: ' + node.label;
            var html = '<dl><dt>ID:</dt><dd>' + node.id + '</dd><dt>Updateable:</dt><dd>' + (node.data.updateable ? 'Yes' : 'No') + '</dd><dt>Attachable:</dt><dd>' + (node.data.attachable ? 'Yes' : 'No') + '</dd>';
            if (node.data.flow) {{ html += '<dt>Flow Order:</dt><dd>' + node.data.flow + '</dd>'; }}
            html += '</dl>';
            var incoming = [], outgoing = [];
            edges.forEach(function(edge) {{ if (edge.to === nodeId) incoming.push(edge); if (edge.from === nodeId) outgoing.push(edge); }});
            if (incoming.length) {{ html += '<h5>Incoming Transitions:</h5><ul>' + incoming.map(function(e){{ return '<li>' + e.data.label + ' (Roles: ' + (e.data.roles || 'None') + ')</li>'; }}).join('') + '</ul>'; }}
            if (outgoing.length) {{ html += '<h5>Outgoing Transitions:</h5><ul>' + outgoing.map(function(e){{ return '<li>' + e.data.label + ' (Roles: ' + (e.data.roles || 'None') + ')</li>'; }}).join('') + '</ul>'; }}
            infoContent.innerHTML = html;
            nodeInfo.className = 'active';
        }}
    }} else if (params.edges.length > 0) {{
        var edgeId = params.edges[0], edge = edges.get(edgeId);
        if (edge && edge.data) {{
            infoTitle.textContent = 'Transition: ' + edge.data.label;
            var fromNode = nodes.get(edge.from), toNode = nodes.get(edge.to);
            infoContent.innerHTML = '<dl><dt>Display Name:</dt><dd>' + edge.data.displayName + '</dd><dt>Roles:</dt><dd>' + (edge.data.roles || 'None') + '</dd><dt>From:</dt><dd>' + (edge.from === 'start' ? 'New Record' : (fromNode ? fromNode.label : edge.from)) + '</dd><dt>To:</dt><dd>' + (toNode ? toNode.label : edge.to) + '</dd></dl>';
            nodeInfo.className = 'active';
        }}
    }} else {{
        nodeInfo.className = '';
    }}
}});

function togglePhysics() {{
    physicsEnabled = !physicsEnabled;
    network.setOptions({{ physics: {{ enabled: physicsEnabled }} }});
}}
var selfTransitionsShown = false;
window.setSelfTransitions = function(show) {{
    selfTransitionsShown = !!show;
    if (window.TrackerBuilder) {{ TrackerBuilder.selfShown = selfTransitionsShown; }}
    edges.forEach(function(edge) {{
        if (edge.dashes) {{ edges.update({{ id: edge.id, hidden: !selfTransitionsShown }}); }}
    }});
}};
function toggleSelfTransitions() {{ window.setSelfTransitions(!selfTransitionsShown); }}
function exportGraph() {{
    var canvas = container.querySelector('canvas');
    if (!canvas) {{ alert('Canvas not ready'); return; }}
    var a = document.createElement('a');
    a.href = canvas.toDataURL('image/png');
    a.download = '{tslug}_workflow.png';
    a.click();
}}
function releaseHierarchy() {{
    var positions = network.getPositions();
    network.setOptions({{ layout: {{ hierarchical: {{ enabled: false }} }} }});
    nodes.update(Object.keys(positions).map(function(id) {{ return {{ id: id, x: positions[id].x, y: positions[id].y, fixed: false }}; }}));
}}
function isolatedNodeIds() {{
    var degree = {{}};
    nodes.getIds().forEach(function(id) {{ degree[id] = 0; }});
    edges.forEach(function(edge) {{
        if (edge.from === edge.to) {{ return; }}
        if (degree[edge.from] !== undefined) degree[edge.from]++;
        if (degree[edge.to] !== undefined) degree[edge.to]++;
    }});
    return nodes.getIds().filter(function(id) {{ return !degree[id]; }});
}}
function flowShape(isolated) {{
    var isIso = {{}}; isolated.forEach(function(id) {{ isIso[id] = true; }});
    var positions = network.getPositions(), perLevel = {{}};
    Object.keys(positions).forEach(function(id) {{
        if (isIso[id]) return;
        var level = Math.round(positions[id].x);
        perLevel[level] = (perLevel[level] || 0) + 1;
    }});
    var levels = Object.keys(perLevel);
    return {{ levels: levels.length, busiest: levels.reduce(function(m, l) {{ return Math.max(m, perLevel[l]); }}, 0) }};
}}
var ISOLATED_ROW = 70;
function gridIsolated(isolated) {{
    if (!isolated.length) return;
    var isIso = {{}}; isolated.forEach(function(id) {{ isIso[id] = true; }});
    var flow = nodes.getIds().filter(function(id) {{ return !isIso[id]; }});
    var positions = network.getPositions();
    var left = 0, flowWidth = 0, bottom = -140;
    if (flow.length) {{
        var xs = flow.map(function(id) {{ return positions[id].x; }});
        var ys = flow.map(function(id) {{ return positions[id].y; }});
        left = Math.min.apply(null, xs);
        flowWidth = Math.max.apply(null, xs) - left;
        bottom = Math.max.apply(null, ys);
    }}
    var rowWidth = Math.max(flowWidth, 600), x = left, y = bottom + 140;
    isolated.forEach(function(id) {{
        var box = network.getBoundingBox(id);
        var width = (box && box.right - box.left) || 120;
        if (x > left && (x + width) > (left + rowWidth)) {{ x = left; y += ISOLATED_ROW; }}
        nodes.update({{ id: id, x: x + width / 2, y: y, fixed: false }});
        x += width + 26;
    }});
}}
function clamp(v, lo, hi) {{ return Math.max(lo, Math.min(hi, v)); }}
function assignLevels() {{
    var ids = nodes.getIds(), outgoing = {{}}, incoming = {{}};
    ids.forEach(function(id) {{ outgoing[id] = []; incoming[id] = 0; }});
    edges.forEach(function(edge) {{
        if (edge.from === edge.to) return;
        if (!outgoing[edge.from] || incoming[edge.to] === undefined) return;
        outgoing[edge.from].push(edge.to);
        incoming[edge.to]++;
    }});
    var level = {{}}, queue = ids.filter(function(id) {{ return !incoming[id]; }});
    if (!queue.length && ids.length) {{ queue = [ids.indexOf(initialStatusId) >= 0 ? initialStatusId : ids[0]]; }}
    queue.forEach(function(id) {{ level[id] = 0; }});
    for (var i = 0; i < queue.length; i++) {{
        var here = queue[i];
        outgoing[here].forEach(function(next) {{
            if (level[next] !== undefined) return;
            level[next] = level[here] + 1;
            queue.push(next);
        }});
    }}
    nodes.update(ids.map(function(id) {{ return {{ id: id, level: level[id] === undefined ? 0 : level[id] }}; }}));
}}
function wrapCrowdedLevels(availableHeight) {{
    var positions = network.getPositions(), byLevel = {{}};
    nodes.get().forEach(function(node) {{
        var level = node.level || 0;
        (byLevel[level] = byLevel[level] || []).push(node.id);
    }});
    var rowPitch = 62, capacity = Math.max(3, Math.floor(availableHeight / rowPitch));
    var levels = Object.keys(byLevel).sort(function(a, b) {{ return a - b; }});
    if (!levels.some(function(l) {{ return byLevel[l].length > capacity; }})) return;
    var cursor = 0;
    levels.forEach(function(level) {{
        var column = byLevel[level].sort(function(a, b) {{ return positions[a].y - positions[b].y; }});
        var widest = column.reduce(function(m, id) {{ var box = network.getBoundingBox(id); return Math.max(m, (box && box.right - box.left) || 120); }}, 0);
        var rows = Math.min(column.length, capacity), columnPitch = widest + 60, top = -((rows - 1) * rowPitch) / 2;
        column.forEach(function(id, idx) {{
            nodes.update({{ id: id, x: cursor + Math.floor(idx / rows) * columnPitch, y: top + (idx % rows) * rowPitch, fixed: false }});
        }});
        cursor += Math.ceil(column.length / rows) * columnPitch + 80;
    }});
}}
function applyLayout() {{
    var isolated = isolatedNodeIds(), isIso = {{}};
    isolated.forEach(function(id) {{ isIso[id] = true; }});
    var parkedNodes = nodes.get(isolated);
    var parkedEdges = edges.get({{ filter: function(e) {{ return isIso[e.from] || isIso[e.to]; }} }});
    var selected = network.getSelectedNodes();
    if (parkedNodes.length) {{
        edges.remove(parkedEdges.map(function(e) {{ return e.id; }}));
        nodes.remove(isolated);
    }}
    if (nodes.length) {{
        assignLevels();
        network.setOptions({{ layout: {{ hierarchical: hierarchicalLayout(100, 200) }} }});
        var shape = flowShape([]);
        var canvas = container.getBoundingClientRect();
        var reserved = 120 + (isolated.length ? 140 + ISOLATED_ROW * 2 : 0);
        var nodeSpacing = clamp((canvas.height - reserved) / Math.max(shape.busiest, 1), 55, 200);
        var levelSeparation = clamp((canvas.width - 160) / Math.max(shape.levels - 1, 1), 170, 320);
        network.setOptions({{ layout: {{ hierarchical: hierarchicalLayout(nodeSpacing, levelSeparation) }} }});
        releaseHierarchy();
        wrapCrowdedLevels(canvas.height - 100);
    }} else {{
        network.setOptions({{ layout: {{ hierarchical: {{ enabled: false }} }} }});
    }}
    if (parkedNodes.length) {{
        nodes.add(parkedNodes);
        edges.add(parkedEdges);
        network.redraw();
        gridIsolated(isolated);
        if (selected.length) network.selectNodes(selected, false);
    }}
    network.fit();
}}
function relayout() {{ applyLayout(); }}
network.once('afterDrawing', function() {{ applyLayout(); }});
if (builderEnabled) {{
    TrackerBuilder.selfShown = false;
    TrackerBuilder.init(builderConfig, network, nodes, edges);
}}
</script>"##,
        tid = t.id,
        tname = esc(&t.name),
        tmod = esc(&t.module),
        tslug = esc(&t.slug),
        n_cnt = nodes.len(),
        e_cnt = edges.len(),
        init_name = esc(&init_name),
        ttype = esc(ttype),
        can_edit_js = if can_edit { "true" } else { "false" }
    );
    ctx.html(&format!("Workflow Graph - {}", t.name), body).await
}

// ---------------- 9 JSON Builder POST endpoints ----------------

pub async fn builder_action(ctx: Ctx, act: &str) -> Response {
    if ctx.method != "POST" {
        return json_err(StatusCode::METHOD_NOT_ALLOWED, "POST required");
    }
    let payload = parse_payload(&ctx);
    let tid = opt_id_val(payload.get("tracker_id")).or_else(|| ctx.pi("tracker_id")).unwrap_or(0);
    let db = &ctx.st.db.clone();
    let Some(t) = Tracker::by_id(db, tid).await else {
        return json_err(StatusCode::NOT_FOUND, "Tracker not found");
    };
    if !can_build(&ctx, &t).await {
        return json_err(StatusCode::FORBIDDEN, &format!("You need a Developer role on module {} to edit this tracker", t.module));
    }

    match act {
        "builder_add_fields" => {
            let mut next_order = ordered_fields(&t).iter().filter_map(|f| i(f, "field_order")).max().unwrap_or(0) + 1;
            let mut added: Vec<String> = vec![];
            let mut skipped: Vec<String> = vec![];
            if let Some(Value::Array(items)) = payload.get("fields") {
                for f in items {
                    let name = f.get("name").map(vs).unwrap_or_default().trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    if !is_valid_col(&name) {
                        skipped.push(format!("{name} (not a valid column name)"));
                        continue;
                    }
                    if db.count("select count(*) from portal_tracker_field where tracker_id=:t and name=:n", &row! {"t"=>t.id,"n"=>name.clone()}).await.unwrap_or(1) > 0 {
                        skipped.push(format!("{name} (already exists)"));
                        continue;
                    }
                    let label = {
                        let l = f.get("label").map(vs).unwrap_or_default().trim().to_string();
                        if l.is_empty() { name.clone() } else { l }
                    };
                    let ft = {
                        let x = f.get("field_type").map(vs).unwrap_or_default().trim().to_string();
                        if x.is_empty() { "Text".into() } else { x }
                    };
                    match db
                        .insert("portal_tracker_field", &row! {"tracker_id"=>t.id,"name"=>name.clone(),"label"=>label,"field_type"=>ft,"field_order"=>next_order})
                        .await
                    {
                        Ok(fid) => {
                            crate::tracker::create_default_error_checks(db, fid).await;
                            next_order += 1;
                            added.push(name);
                        }
                        Err(e) => skipped.push(format!("{name} ({e})")),
                    }
                }
            }
            let mut dbnote: Option<String> = None;
            if !added.is_empty() {
                if let Some(t2) = Tracker::by_id(db, t.id).await {
                    if let Err(e) = t2.updatedb(db).await {
                        dbnote = Some(format!("Fields were saved but the database columns could not be created: {e} - use Update DB on the tracker page."));
                    }
                }
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await, "added": added, "skipped": skipped, "dbnote": dbnote})).await
        }

        "builder_add_roles" => {
            let mut added: Vec<String> = vec![];
            let mut skipped: Vec<String> = vec![];
            if let Some(Value::Array(items)) = payload.get("roles") {
                for r in items {
                    let name = r.get("name").map(vs).unwrap_or_default().trim().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    if name.contains(',') {
                        skipped.push(format!("{name} (a role name cannot contain a comma)"));
                        continue;
                    }
                    if db.count("select count(*) from portal_tracker_role where tracker_id=:t and name=:n", &row! {"t"=>t.id,"n"=>name.clone()}).await.unwrap_or(1) > 0 {
                        skipped.push(format!("{name} (already exists)"));
                        continue;
                    }
                    let rtype = {
                        let x = r.get("role_type").map(vs).unwrap_or_default().trim().to_string();
                        if x.is_empty() { "User Role".into() } else { x }
                    };
                    if !ROLE_TYPES.contains(&rtype.as_str()) {
                        skipped.push(format!("{name} (unknown role type '{rtype}')"));
                        continue;
                    }
                    let rule = r.get("role_rule").map(vs).unwrap_or_default().trim().to_string();
                    let rule_v = if rule.is_empty() { Value::Null } else { json!(rule) };
                    match db
                        .insert("portal_tracker_role", &row! {"tracker_id"=>t.id,"name"=>name.clone(),"role_type"=>rtype,"role_rule"=>rule_v,"last_updated"=>now_str()})
                        .await
                    {
                        Ok(_) => added.push(name),
                        Err(e) => skipped.push(format!("{name} ({e})")),
                    }
                }
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await, "added": added, "skipped": skipped})).await
        }

        "builder_save_field_group" => {
            let Some(fid) = opt_id_val(payload.get("field_id")) else {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That field is not on this tracker.");
            };
            let Some(group) = t.fields.iter().find(|f| i(f, "id") == Some(fid)) else {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That field is not on this tracker.");
            };
            let gname = s(group, "name");
            if s(group, "field_type") != "FieldGroup" {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("{gname} is not a Field Group."));
            }
            let by_name: HashMap<String, &Row> = t.fields.iter().map(|f| (s(f, "name"), f)).collect();
            let mut wanted: Vec<String> = vec![];
            for m in str_vec(payload.get("members")) {
                if !wanted.contains(&m) {
                    wanted.push(m);
                }
            }
            let unknown: Vec<String> = wanted.iter().filter(|m| !by_name.contains_key(*m)).cloned().collect();
            if !unknown.is_empty() {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Not fields on this tracker: {}", unknown.join(", ")));
            }
            if wanted.contains(&gname) {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "A group cannot contain itself.");
            }
            // Cycle check across nested FieldGroups
            let mut seen: HashSet<String> = HashSet::new();
            let mut stack: Vec<String> = wanted.iter().filter(|m| by_name.get(*m).map(|f| s(f, "field_type") == "FieldGroup").unwrap_or(false)).cloned().collect();
            while let Some(cur) = stack.pop() {
                if cur == gname {
                    return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("That would make {gname} contain itself through a nested group."));
                }
                if !seen.insert(cur.clone()) {
                    continue;
                }
                let mems = if cur == gname {
                    wanted.clone()
                } else {
                    by_name.get(&cur).map(|f| csv(&s(f, "field_options"))).unwrap_or_default()
                };
                for m in mems {
                    if by_name.get(&m).map(|f| s(f, "field_type") == "FieldGroup").unwrap_or(false) {
                        stack.push(m);
                    }
                }
            }
            let existing = csv(&s(group, "field_options"));
            let mut final_list: Vec<String> = existing.iter().filter(|x| wanted.contains(x)).cloned().collect();
            for w in wanted {
                if !final_list.contains(&w) {
                    final_list.push(w);
                }
            }
            if let Err(e) = db.update("portal_tracker_field", fid, &row! {"field_options"=>final_list.join(",")}).await {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the group: {e}"));
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await})).await
        }

        "builder_save_field_order" => {
            let names = str_vec(payload.get("field_names"));
            for (idx, fname) in names.iter().enumerate() {
                let _ = db
                    .exec(
                        "update portal_tracker_field set field_order=:o where tracker_id=:t and name=:n",
                        &row! {"o"=>(idx as i64) + 1,"t"=>t.id,"n"=>fname.clone()},
                    )
                    .await;
            }
            let t2 = Tracker::by_id(db, t.id).await.unwrap_or(t);
            let canonical: Vec<String> = ordered_fields(&t2).iter().map(|f| s(f, "name")).collect();
            for st in &t2.statuses {
                if let Some(sid) = i(st, "id") {
                    let df = filter_in_order(&canonical, &csv(&s(st, "displayfields")));
                    let ef = filter_in_order(&canonical, &csv(&s(st, "editfields")));
                    let _ = db.update("portal_tracker_status", sid, &row! {"displayfields"=>df,"editfields"=>ef}).await;
                }
            }
            for tr in &t2.transitions {
                let df = filter_in_order(&canonical, &csv(&s(&tr.row, "displayfields")));
                let ef = filter_in_order(&canonical, &csv(&s(&tr.row, "editfields")));
                let _ = db.update("portal_tracker_transition", tr.id, &row! {"displayfields"=>df,"editfields"=>ef}).await;
            }
            let mut tup = Row::new();
            for &prop in TRACKER_FIELD_LISTS {
                tup.insert(prop.into(), json!(filter_in_order(&canonical, &csv(&s(&t2.row, prop)))));
            }
            let _ = db.update("portal_tracker", t2.id, &tup).await;
            let fresh = Tracker::by_id(db, t2.id).await.unwrap_or(t2);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await})).await
        }

        "builder_save_tracker_lists" => {
            let canonical: Vec<String> = ordered_fields(&t).iter().map(|f| s(f, "name")).collect();
            let mut tup = Row::new();
            if let Some(Value::Object(lists)) = payload.get("lists") {
                for &prop in TRACKER_FIELD_LISTS {
                    if let Some(v) = lists.get(prop) {
                        let wanted = str_vec(Some(v));
                        tup.insert(prop.into(), json!(filter_in_order(&canonical, &wanted)));
                    }
                }
            }
            if !tup.is_empty() {
                if let Err(e) = db.update("portal_tracker", t.id, &tup).await {
                    return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the tracker field lists: {e}"));
                }
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await})).await
        }

        "builder_save_status" => {
            let name = payload.get("name").map(vs).unwrap_or_default().trim().to_string();
            if name.is_empty() {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "A status needs a name");
            }
            let sid = opt_id_val(payload.get("id"));
            if let Some(id_val) = sid {
                if !t.statuses.iter().any(|st| i(st, "id") == Some(id_val)) {
                    return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That status belongs to another tracker");
                }
            }
            if t.statuses.iter().any(|st| s(st, "name") == name && i(st, "id") != sid) {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("This tracker already has a status called '{name}'"));
            }
            let canonical: Vec<String> = ordered_fields(&t).iter().map(|f| s(f, "name")).collect();
            let mut d = row! {"tracker_id"=>t.id,"name"=>name};
            if let Some(v) = payload.get("updateable") {
                d.insert("updateable".into(), json!(v.as_bool().unwrap_or(false)));
            }
            if let Some(v) = payload.get("attachable") {
                d.insert("attachable".into(), json!(v.as_bool().unwrap_or(false)));
            }
            if payload.contains_key("view_fields") || payload.contains_key("edit_fields") {
                d.insert("displayfields".into(), json!(filter_in_order(&canonical, &str_vec(payload.get("view_fields")))));
                d.insert("editfields".into(), json!(filter_in_order(&canonical, &str_vec(payload.get("edit_fields")))));
            }
            if payload.contains_key("editroles") {
                d.insert("editroles".into(), json!(str_vec(payload.get("editroles")).join(",")));
            }
            if payload.contains_key("updateallowedroles") {
                d.insert("updateallowedroles".into(), json!(str_vec(payload.get("updateallowedroles")).join(",")));
            }
            let saved_id = match sid {
                Some(id_val) => match db.update("portal_tracker_status", id_val, &d).await {
                    Ok(_) => id_val,
                    Err(e) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the status: {e}")),
                },
                None => match db.insert("portal_tracker_status", &d).await {
                    Ok(nid) => nid,
                    Err(e) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the status: {e}")),
                },
            };
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await, "saved_id": saved_id.to_string()})).await
        }

        "builder_delete_status" => {
            let Some(sid) = opt_id_val(payload.get("id")) else {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "Status not found on this tracker");
            };
            let Some(st_row) = t.statuses.iter().find(|st| i(st, "id") == Some(sid)) else {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "Status not found on this tracker");
            };
            let st_name = s(st_row, "name");
            let mut used_by: Vec<String> = vec![];
            if t.initial_status() == Some(sid) {
                used_by.push("it is this tracker's initial status".into());
            }
            let as_next: Vec<String> = t.transitions.iter().filter(|tr| tr.next == Some(sid)).map(|tr| tr.name.clone()).collect();
            if !as_next.is_empty() {
                used_by.push(format!("it is the destination of: {}", as_next.join(", ")));
            }
            let as_prev: Vec<String> = t.transitions.iter().filter(|tr| tr.prev.contains(&sid)).map(|tr| tr.name.clone()).collect();
            if !as_prev.is_empty() {
                used_by.push(format!("transitions start from it: {}", as_prev.join(", ")));
            }
            if !used_by.is_empty() {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Cannot delete '{st_name}' - {}", used_by.join("; ")));
            }
            if let Err(e) = db.delete("portal_tracker_status", sid).await {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not delete the status: {e}"));
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await})).await
        }

        "builder_save_transition" => {
            let name = payload.get("name").map(vs).unwrap_or_default().trim().to_string();
            if name.is_empty() {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "A transition needs a name");
            }
            let trid = opt_id_val(payload.get("id"));
            if let Some(id_val) = trid {
                if !t.transitions.iter().any(|tr| tr.id == id_val) {
                    return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That transition belongs to another tracker");
                }
            }
            if t.transitions.iter().any(|tr| tr.name == name && Some(tr.id) != trid) {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("This tracker already has a transition called '{name}'"));
            }
            let same_status = payload.get("same_status").and_then(|v| v.as_bool()).unwrap_or(false);
            let next_status_id = if same_status {
                None
            } else {
                let nid = opt_id_val(payload.get("next_status_id"));
                if let Some(n) = nid {
                    if !t.statuses.iter().any(|st| i(st, "id") == Some(n)) {
                        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "Destination status not found on this tracker");
                    }
                }
                nid
            };
            let mut postprocess_set: Option<Option<i64>> = None;
            if let Some(ppv) = payload.get("postprocess_id") {
                let raw = vs(ppv).trim().to_string();
                if raw.is_empty() {
                    postprocess_set = Some(None);
                } else {
                    let Ok(ppid) = raw.parse::<i64>() else {
                        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That postprocess page id is not a number");
                    };
                    let Ok(Some(pg)) = db.get(db::page_t(), ppid).await else {
                        return json_err(StatusCode::UNPROCESSABLE_ENTITY, "That postprocess page no longer exists");
                    };
                    if s(&pg, "module") != t.module {
                        return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("A postprocess page has to live in this tracker's module ({})", t.module));
                    }
                    postprocess_set = Some(Some(ppid));
                }
            }

            let canonical: Vec<String> = ordered_fields(&t).iter().map(|f| s(f, "name")).collect();
            let mut d = row! {"tracker_id"=>t.id,"name"=>name,"same_status"=>same_status,"next_status_id"=>next_status_id};
            if let Some(dv) = payload.get("display_name") {
                let dt = vs(dv).trim().to_string();
                d.insert("display_name".into(), if dt.is_empty() { Value::Null } else { json!(dt) });
            }
            if payload.contains_key("view_fields") || payload.contains_key("edit_fields") {
                d.insert("displayfields".into(), json!(filter_in_order(&canonical, &str_vec(payload.get("view_fields")))));
                d.insert("editfields".into(), json!(filter_in_order(&canonical, &str_vec(payload.get("edit_fields")))));
            }
            if let Some(pp) = postprocess_set {
                d.insert("postprocess_id".into(), match pp { Some(x) => json!(x), None => Value::Null });
            }

            let saved_id = match trid {
                Some(id_val) => match db.update("portal_tracker_transition", id_val, &d).await {
                    Ok(_) => id_val,
                    Err(e) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the transition: {e}")),
                },
                None => match db.insert("portal_tracker_transition", &d).await {
                    Ok(nid) => nid,
                    Err(e) => return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not save the transition: {e}")),
                },
            };

            if payload.contains_key("prev_status_ids") {
                let _ = db
                    .exec("delete from portal_tracker_transition_portal_tracker_status where portal_tracker_transition_prev_status_id=:tid", &row! {"tid"=>saved_id})
                    .await;
                for pid_s in str_vec(payload.get("prev_status_ids")) {
                    if let Ok(pid) = pid_s.parse::<i64>() {
                        if t.statuses.iter().any(|st| i(st, "id") == Some(pid)) {
                            let _ = db
                                .exec(
                                    "insert into portal_tracker_transition_portal_tracker_status (portal_tracker_transition_prev_status_id, portal_tracker_status_id) values (:t,:s)",
                                    &row! {"t"=>saved_id,"s"=>pid},
                                )
                                .await;
                        }
                    }
                }
            }
            if payload.contains_key("role_ids") {
                let _ = db
                    .exec("delete from portal_tracker_transition_portal_tracker_role where portal_tracker_transition_roles_id=:tid", &row! {"tid"=>saved_id})
                    .await;
                for rid_s in str_vec(payload.get("role_ids")) {
                    if let Ok(rid) = rid_s.parse::<i64>() {
                        if t.roles.iter().any(|r| i(r, "id") == Some(rid)) {
                            let _ = db
                                .exec(
                                    "insert into portal_tracker_transition_portal_tracker_role (portal_tracker_transition_roles_id, portal_tracker_role_id) values (:t,:r)",
                                    &row! {"t"=>saved_id,"r"=>rid},
                                )
                                .await;
                        }
                    }
                }
            }

            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await, "saved_id": saved_id.to_string()})).await
        }

        "builder_delete_transition" => {
            let Some(trid) = opt_id_val(payload.get("id")) else {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "Transition not found on this tracker");
            };
            if !t.transitions.iter().any(|tr| tr.id == trid) {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, "Transition not found on this tracker");
            }
            let _ = db
                .exec("delete from portal_tracker_transition_portal_tracker_status where portal_tracker_transition_prev_status_id=:tid", &row! {"tid"=>trid})
                .await;
            let _ = db
                .exec("delete from portal_tracker_transition_portal_tracker_role where portal_tracker_transition_roles_id=:tid", &row! {"tid"=>trid})
                .await;
            if let Err(e) = db.delete("portal_tracker_transition", trid).await {
                return json_err(StatusCode::UNPROCESSABLE_ENTITY, &format!("Could not delete the transition: {e}"));
            }
            let fresh = Tracker::by_id(db, t.id).await.unwrap_or(t);
            ctx.json(json!({"ok": true, "model": builder_model(db, &fresh).await})).await
        }

        _ => json_err(StatusCode::NOT_FOUND, "Unknown builder action"),
    }
}

fn is_valid_col(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return false };
    (first.is_ascii_alphabetic() || first == '_') && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_col_and_script_json_escaping() {
        assert!(is_valid_col("event_start"));
        assert!(is_valid_col("_id2"));
        assert!(!is_valid_col("2bad"));
        assert!(!is_valid_col("bad-name"));
        assert_eq!(script_json(&json!("</script>")), "\"<\\/script>\"");
    }

    #[test]
    fn filter_in_order_preserves_canonical_order() {
        let canon: Vec<String> = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let wanted: Vec<String> = vec!["d".into(), "b".into(), "missing".into()];
        assert_eq!(filter_in_order(&canon, &wanted), "b,d");
    }
}
