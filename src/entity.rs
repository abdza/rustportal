//! Declarative description of the portal's system tables. One definition
//! drives both the database schema (created/extended at start-up, like
//! Grails `dbCreate: update`) and the generic admin CRUD screens.

use crate::db::Db;
use anyhow::Result;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum K {
    Str,
    Text,
    Code,
    Bool,
    Int,
    Date,
    Ref(&'static str),
    Choice(&'static [&'static str]),
    Password,
    /// many-to-many: (other entity, join table, own column, other column)
    Multi(&'static str, &'static str, &'static str, &'static str),
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Scope {
    /// only portal administrators
    Global,
    /// row carries the module name in this column
    Module(&'static str),
    /// module is found through a parent row: (fk column, parent entity)
    Via(&'static str, &'static str),
}

#[derive(Clone, Debug)]
pub struct F {
    pub name: &'static str,
    pub kind: K,
    pub hidden: bool,
}

#[derive(Clone, Debug)]
pub struct Entity {
    pub name: &'static str,
    pub table: &'static str,
    pub label: &'static str,
    pub fields: Vec<F>,
    pub list: &'static [&'static str],
    pub display: &'static str,
    pub scope: Scope,
    pub children: &'static [(&'static str, &'static str)],
    pub readonly: bool,
    pub order: &'static str,
}

impl Entity {
    fn new(name: &'static str, table: &'static str, label: &'static str, display: &'static str, scope: Scope) -> Entity {
        Entity { name, table, label, fields: vec![], list: &[], display, scope, children: &[], readonly: false, order: "id desc" }
    }
    fn f(mut self, fs: &[(&'static str, K)]) -> Self {
        for (n, k) in fs {
            self.fields.push(F { name: n, kind: *k, hidden: false });
        }
        self
    }
    fn h(mut self, fs: &[(&'static str, K)]) -> Self {
        for (n, k) in fs {
            self.fields.push(F { name: n, kind: *k, hidden: true });
        }
        self
    }
    fn list(mut self, l: &'static [&'static str]) -> Self {
        self.list = l;
        self
    }
    fn children(mut self, c: &'static [(&'static str, &'static str)]) -> Self {
        self.children = c;
        self
    }
    fn readonly(mut self) -> Self {
        self.readonly = true;
        self
    }
    fn order(mut self, o: &'static str) -> Self {
        self.order = o;
        self
    }
    pub fn field(&self, name: &str) -> Option<&F> {
        self.fields.iter().find(|f| f.name == name)
    }
}

pub fn label_of(name: &str) -> String {
    let n = name.strip_suffix("_id").unwrap_or(name).replace('_', " ");
    let mut c = n.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => n,
    }
}

pub const FIELD_TYPES: &[&str] = &[
    "Text", "Text Area", "Integer", "Number", "Date", "DateTime", "Checkbox", "Drop Down", "MultiSelect", "User", "File",
    "Branch", "BelongsTo", "HasMany", "TreeNode", "Hidden", "FieldGroup",
];

pub fn entities() -> Vec<Entity> {
    use Scope::*;
    use K::*;
    vec![
        Entity::new("user", "portal_user", "User", "name", Global)
            .f(&[("userid", Str), ("name", Str), ("email", Str), ("password", Password), ("is_admin", Bool), ("is_active", Bool),
                ("reset_password", Bool), ("lanid", Str), ("role", Str), ("roletargetid", Int), ("state", Str), ("profile_id", Int),
                ("handphone", Str), ("directline", Str), ("company_handphone", Str), ("address", Text), ("emergency_name", Str),
                ("emergency_contact", Str), ("profilepic_id", Ref("fileLink")), ("date_joined", Date)])
            .h(&[("secretquestion", Str), ("secretanswer", Str), ("password5", Text), ("lanidexception", Bool), ("resetexception", Bool),
                ("treesdate", Date), ("lastlogin", Date), ("last_updated", Date), ("last_info_update", Date), ("last_reminder", Date),
                ("active_session_id", Str), ("active_session_updated", Date),
                ("telegram_chat_id", Str), ("telegram_username", Str), ("telegram_linked_at", Date),
                ("telegram_link_token", Str), ("telegram_token_expiry", Date)])
            .list(&["userid", "name", "email", "is_admin", "is_active", "lastlogin"])
            .children(&[("userRole", "user_id"), ("portalTreeNodeUser", "user_id")])
            .order("name"),
        Entity::new("userRole", "user_role", "User Role", "role", Module("module"))
            .f(&[("user_id", Ref("user")), ("module", Str), ("role", Str)])
            .list(&["user_id", "module", "role"]),
        Entity::new("userLog", "user_log", "User Log", "description", Global)
            .f(&[("date", Date), ("category", Str), ("description", Text), ("user_id", Ref("user")), ("targetuser_id", Ref("user")), ("ipaddr", Str)])
            .list(&["date", "category", "description", "user_id", "targetuser_id", "ipaddr"])
            .readonly(),
        Entity::new("portalSetting", "portal_setting", "Setting", "name", Module("module"))
            .f(&[("name", Str), ("module", Str), ("type", Choice(&["Text", "Array", "Hashmap", "Date", "Number", "Xml"])),
                ("datum_type", Choice(&["", "String", "Integer", "Float", "Date", "DateTime", "Array"])), ("text", Text), ("date_value", Date), ("number", Int)])
            .list(&["name", "module", "type", "text", "date_value", "number", "datum_type"])
            .order("id"),
        Entity::new("portalPage", crate::db::page_t(), "Page", "title", Module("module"))
            .f(&[("title", Str), ("slug", Str), ("module", Str), ("published", Bool), ("requirelogin", Bool), ("allowedroles", Str),
                ("runable", Bool), ("redirectafter", Str), ("fullpage", Bool), ("render", Choice(&["", "Default", "HTML", "XML", "JSON", "File", "XLSX"])),
                ("side_menu", Str), ("content", Code), ("preprocess", Code)])
            .h(&[("last_updated", Date)])
            .list(&["title", "slug", "module", "published", "requirelogin", "allowedroles", "runable"])
            .children(&[("portalPageData", "page_id")])
            .order("module,slug"),
        Entity::new("portalPageData", crate::db::page_data_t(), "Page Data Source", "name", Via("page_id", "portalPage"))
            .f(&[("page_id", Ref("portalPage")), ("name", Str), ("return_one", Bool), ("query", Code)])
            .list(&["page_id", "name", "return_one"]),
        Entity::new("portalTracker", "portal_tracker", "Tracker", "name", Module("module"))
            .f(&[("name", Str), ("module", Str), ("slug", Str), ("tracker_type", Choice(&["Tracker", "Statement", "DataStore"])),
                ("initial_status_id", Ref("portalTrackerStatus")), ("listfields", Text), ("allowedroles", Str), ("hiddenlistfields", Text),
                ("excelfields", Text), ("searchfields", Text), ("filterfields", Text), ("row_validation_fields", Text), ("allowadd", Bool),
                ("downloadexcel", Bool), ("excel_audit", Bool), ("defaultsort", Str), ("defaultlimit", Int), ("rolesort", Str),
                ("tickactions", Str), ("actionbuttons", Str), ("anonymous_list", Bool), ("anonymous_view", Bool), ("require_login", Bool),
                ("condition_q", Text), ("rowclassval", Text), ("datatable", Str), ("trailtable", Str), ("postprocess_id", Ref("portalPage")),
                ("defaultfield_id", Ref("portalTrackerField")), ("side_menu", Str)])
            .list(&["module", "slug", "name", "tracker_type"])
            .children(&[("portalTrackerField", "tracker_id"), ("portalTrackerStatus", "tracker_id"), ("portalTrackerTransition", "tracker_id"),
                ("portalTrackerRole", "tracker_id"), ("portalTrackerEmail", "tracker_id"), ("portalTrackerFlow", "tracker_id"),
                ("portalTrackerIndex", "tracker_id"), ("portalTrackerData", "tracker_id")])
            .order("module,slug"),
        Entity::new("portalTrackerField", "portal_tracker_field", "Tracker Field", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("label", Str), ("field_type", Choice(FIELD_TYPES)), ("field_options", Code),
                ("field_format", Code), ("field_default", Code), ("hyperscript", Code), ("field_display", Text), ("field_query", Code),
                ("classes", Str), ("hide_heading", Bool), ("params_override", Bool), ("url_value", Bool), ("is_encrypted", Bool),
                ("role_query", Bool), ("encode_exception", Bool), ("suppress_follow_link", Bool), ("field_description", Text),
                ("field_tooltip", Str), ("field_placeholder", Text), ("row_validation_regex", Str), ("field_order", Int)])
            .list(&["tracker_id", "name", "label", "field_type", "field_order"])
            .children(&[("portalTrackerError", "field_id")])
            .order("tracker_id,field_order nulls last,name"),
        Entity::new("portalTrackerError", "portal_tracker_error", "Field Error Check", "error_type", Via("field_id", "portalTrackerField"))
            .f(&[("field_id", Ref("portalTrackerField")), ("description", Str),
                ("error_type", Choice(&["Not Empty", "Unique", "Format", "E-mail", "Custom"])), ("format", Str), ("error_msg", Code),
                ("error_function", Code), ("allow_submission", Bool)])
            .list(&["field_id", "description", "error_type", "allow_submission"]),
        Entity::new("portalTrackerStatus", "portal_tracker_status", "Tracker Status", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("displayfields", Text), ("editroles", Text), ("editfields", Text),
                ("updateable", Bool), ("attachable", Bool), ("emailonupdate_id", Ref("portalTrackerEmail")), ("runonupdate_id", Ref("portalPage")),
                ("updateallowedroles", Str), ("suppressupdatebutton", Bool), ("flow", Int), ("actiontransitions", Bool), ("composite_statuses", Text)])
            .list(&["tracker_id", "name", "flow", "updateable"])
            .order("tracker_id,flow nulls last,name"),
        Entity::new("portalTrackerTransition", "portal_tracker_transition", "Tracker Transition", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("display_name", Str),
                ("roles", Multi("portalTrackerRole", "portal_tracker_transition_portal_tracker_role", "portal_tracker_transition_roles_id", "portal_tracker_role_id")),
                ("prev_status", Multi("portalTrackerStatus", "portal_tracker_transition_portal_tracker_status", "portal_tracker_transition_prev_status_id", "portal_tracker_status_id")),
                ("next_status_id", Ref("portalTrackerStatus")), ("same_status", Bool), ("editfields", Text), ("displayfields", Text),
                ("requiredfields", Text), ("richtextfields", Text), ("enabledcondition", Code), ("updatetrails", Code),
                ("postprocess_id", Ref("portalPage")), ("redirect_after", Str), ("gotoprevstatuslist", Bool), ("submitbuttontext", Str),
                ("cancelbuttontext", Str), ("cancelbutton", Bool), ("immediate_submission", Bool), ("button_class", Str)])
            .list(&["tracker_id", "name", "prev_status", "next_status_id", "roles"])
            .children(&[("portalTrackerEmail", "transition_id")])
            .order("tracker_id,name"),
        Entity::new("portalTrackerRole", "portal_tracker_role", "Tracker Role", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("role_type", Choice(&["User Role", "Data Compare"])), ("role_rule", Code), ("role_desc", Text)])
            .h(&[("last_updated", Date)])
            .list(&["tracker_id", "name", "role_type", "role_desc"]),
        Entity::new("portalTrackerEmail", "portal_tracker_email", "Tracker Email", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("transition_id", Ref("portalTrackerTransition")),
                ("status_id", Ref("portalTrackerStatus")), ("emailto", Code), ("emailcc", Code), ("body_id", Ref("portalPage"))])
            .list(&["tracker_id", "name", "transition_id", "body_id"]),
        Entity::new("portalTrackerIndex", "portal_tracker_index", "Tracker Index", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("fields", Text)])
            .list(&["tracker_id", "name", "fields"]),
        Entity::new("portalTrackerFlow", "portal_tracker_flow", "Tracker Flow", "name", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("name", Str), ("fields", Text), ("transitions", Text)])
            .list(&["tracker_id", "name", "fields", "transitions"]),
        Entity::new("portalTrackerData", "portal_tracker_data", "Data Update", "id", Via("tracker_id", "portalTracker"))
            .f(&[("tracker_id", Ref("portalTracker")), ("module", Str), ("file_link_id", Ref("fileLink")), ("header_start", Int), ("header_end", Int),
                ("data_row", Int), ("data_end", Int), ("send_email", Bool)])
            .h(&[("path", Str), ("date_created", Date), ("uploaded", Bool), ("sent_email_date", Date), ("messages", Text), ("savedparams", Text),
                ("upload_status", Int), ("uploader_id", Ref("user")), ("excel_password", Str), ("is_tracker_deleting", Bool)])
            .list(&["tracker_id", "module", "upload_status", "path", "header_start", "header_end", "data_row"]),
        Entity::new("portalModule", "portal_module", "Module", "name", Module("name"))
            .f(&[("name", Str), ("title", Str), ("description", Text), ("benefit", Text),
                ("status", Choice(&["", "Active", "Inactive", "Retired"])), ("department", Str), ("category", Str)])
            .list(&["name", "title", "status", "category"])
            .children(&[("portalModuleContact", "module_id"), ("portalModuleAccessReview", "module_id")])
            .order("id"),
        Entity::new("portalModuleContact", "portal_module_contact", "Module Contact", "role", Via("module_id", "portalModule"))
            .f(&[("module_id", Ref("portalModule")), ("user_id", Ref("user")), ("role", Choice(&["Owner", "Maintainer"]))])
            .h(&[("date_created", Date), ("last_updated", Date)])
            .list(&["module_id", "user_id", "role"]),
        Entity::new("portalModuleAccessReview", "portal_module_access_review", "Access Review", "period", Via("module_id", "portalModule"))
            .f(&[("module_id", Ref("portalModule")), ("period", Str), ("status", Choice(&["Pending", "Approved", "Overdue", "Closed"])),
                ("opened_date", Date), ("due_date", Date), ("approval_note", Text)])
            .h(&[("reminders_sent", Int), ("last_reminder_date", Date), ("approved_by_id", Ref("user")), ("approved_date", Date),
                ("approved_snapshot", Text), ("closed_date", Date), ("date_created", Date), ("last_updated", Date)])
            .list(&["module_id", "period", "status", "due_date", "approved_by_id", "approved_date"]),
        Entity::new("portalModuleAccessReviewLog", "portal_module_access_review_log", "Access Review Log", "action", Global)
            .f(&[("review_id", Ref("portalModuleAccessReview")), ("action", Str), ("user_id", Ref("user")), ("role", Str), ("actor_id", Ref("user")),
                ("detail", Text), ("date_created", Date)])
            .list(&["review_id", "date_created", "action", "user_id", "role", "actor_id"])
            .readonly(),
        Entity::new("portalModuleImportLog", "portal_module_import_log", "Module Import Log", "module", Global)
            .f(&[("module", Str), ("staffid", Str), ("staffname", Str), ("remarks", Text), ("diff", Text), ("date_created", Date)])
            .list(&["date_created", "module", "staffid", "staffname", "remarks"])
            .readonly(),
        Entity::new("fileLink", "file_link", "File", "name", Module("module"))
            .f(&[("name", Str), ("module", Str), ("slug", Str), ("allowedroles", Str), ("filegroup", Str), ("sortnum", Int), ("path", Str)])
            .h(&[("tracker_slug", Str), ("tracker_data_id", Int), ("size", Int), ("last_updated", Date), ("last_access", Date)])
            .list(&["name", "module", "slug", "size", "filegroup", "sortnum", "allowedroles", "path"])
            .order("sortnum asc, id asc"),
        Entity::new("portalFileManager", "portal_file_manager", "File Manager", "name", Module("module"))
            .f(&[("name", Str), ("module", Str), ("path", Str)])
            .list(&["name", "module", "path"])
            .order("id asc"),
        Entity::new("portalEmail", "portal_email", "Email", "title", Module("module"))
            .f(&[("title", Str), ("module", Str), ("emailto", Text), ("emailcc", Text), ("body", Code), ("delivery_time", Date), ("email_sent", Bool), ("time_sent", Date)])
            .h(&[("error", Text)])
            .list(&["title", "module", "emailto", "emailcc", "body", "delivery_time", "email_sent", "time_sent"])
            .order("delivery_time desc, id desc"),
        Entity::new("portalScheduler", "portal_scheduler", "Scheduler", "name", Module("module"))
            .f(&[("name", Str), ("module", Str), ("slugs", Text), ("hour_of_day", Str), ("day_of_week", Str), ("day_of_month", Str), ("enabled", Bool), ("catch_up", Bool)])
            .h(&[("hour", Str), ("lastrun", Date), ("last_updated", Date)])
            .list(&["name", "module", "slugs", "hour_of_day", "day_of_week", "day_of_month", "lastrun", "enabled", "catch_up"]),
        Entity::new("portalSchedulerRun", "portal_scheduler_run", "Scheduler Run", "name", Global)
            .f(&[("run_type", Str), ("status", Str), ("scheduler_id", Int), ("tick_id", Int), ("name", Str), ("module", Str), ("slug", Str),
                ("due_slot", Date), ("started", Date), ("finished", Date), ("duration_ms", Int), ("error", Text), ("result", Text),
                ("triggered_by", Str), ("host", Str)])
            .list(&["started", "module", "slug", "name", "status", "duration_ms", "triggered_by", "error"])
            .readonly(),
        Entity::new("portalEndpoint", "portal_endpoint", "Endpoint", "name", Module("module"))
            .f(&[("module", Str), ("slug", Str), ("name", Str), ("description", Text), ("handler_type", Choice(&["CGI", "Proxy"])), ("target", Str),
                ("working_dir", Str), ("env_json", Code), ("header_env_json", Code), ("auth_mode", Choice(&["None", "Session", "Basic", "Token"])),
                ("auth_token", Str), ("allowed_roles", Str), ("realm", Str), ("enabled", Bool), ("timeout_seconds", Int), ("max_body_mb", Int)])
            .h(&[("date_created", Date), ("last_updated", Date)])
            .list(&["module", "slug", "name", "handler_type", "auth_mode", "enabled"]),
        Entity::new("portalErrorLog", "portal_error_log", "Error Log", "errormsg", Global)
            .f(&[("date", Date), ("controller", Str), ("action", Str), ("module", Str), ("slug", Str), ("errormsg", Text), ("params", Text),
                ("user_id", Ref("user")), ("resolved", Bool), ("dateresolved", Date), ("ipaddr", Str), ("uri", Str)])
            .list(&["date", "controller", "action", "module", "slug", "errormsg", "user_id", "resolved"]),
        Entity::new("portalAuditTrail", "portal_audit_trail", "Audit Trail", "uri", Global)
            .f(&[("date", Date), ("controller", Str), ("action", Str), ("module", Str), ("slug", Str), ("params", Text), ("uri", Str),
                ("user_id", Ref("user")), ("realuser_id", Ref("user")), ("ipaddr", Str), ("useragent", Str), ("duration_ms", Int), ("cpu_ms", Int),
                ("status", Int), ("error", Text)])
            .list(&["date", "uri", "user_id", "realuser_id", "ipaddr", "status", "duration_ms"])
            .readonly(),
        Entity::new("portalBackgroundTrail", "portal_background_trail", "Background Job", "name", Global)
            .f(&[("name", Str), ("details", Text), ("user_id", Ref("user")), ("start_date", Date), ("duration_ms", Int), ("cpu_ms", Int),
                ("success", Bool), ("error", Text), ("thread_name", Str)])
            .list(&["start_date", "name", "details", "user_id", "duration_ms", "success", "error"])
            .readonly(),
        Entity::new("portalTree", "portal_tree", "Tree", "name", Module("module"))
            .f(&[("module", Str), ("name", Str), ("valid", Date), ("expire", Date)])
            .h(&[("root_id", Ref("portalTreeNode"))])
            .list(&["module", "name", "root_id", "expire", "valid"])
            .order("module,name"),
        Entity::new("portalTreeNode", "portal_tree_node", "Tree Node", "name", Via("tree_id", "portalTree"))
            .f(&[("tree_id", Ref("portalTree")), ("parent_id", Ref("portalTreeNode")), ("name", Str), ("slug", Str), ("domain", Str),
                ("domainid", Int), ("mainrole", Str), ("hiderole", Str), ("data", Code), ("disabled", Bool)])
            .h(&[("lft", Int), ("rgt", Int)])
            .list(&["tree_id", "name", "slug", "parent_id"])
            .children(&[("portalTreeNodeUser", "node_id")])
            .order("tree_id,lft"),
        Entity::new("portalTreeNodeUser", "portal_tree_node_user", "Tree Node User", "role", Global)
            .f(&[("node_id", Ref("portalTreeNode")), ("user_id", Ref("user")), ("role", Str)])
            .list(&["node_id", "user_id", "role"]),
        Entity::new("jobStatus", "job_status", "Job Status", "job_type", Global)
            .f(&[("job_type", Str), ("status", Str), ("start_time", Date), ("end_time", Date), ("total_records", Int), ("processed_records", Int),
                ("skipped_records", Int), ("error_message", Text), ("module_name", Str), ("cancel_requested", Bool)])
            .list(&["start_time", "job_type", "module_name", "status", "processed_records", "total_records"])
            .readonly(),
    ]
}

fn sqltype(k: K) -> Option<&'static str> {
    Some(match k {
        K::Str | K::Choice(_) | K::Password => "varchar(255)",
        K::Text | K::Code => "text",
        K::Bool => "boolean",
        K::Int => "integer",
        K::Date => "timestamp",
        K::Ref(_) => "bigint",
        K::Multi(..) => return None,
    })
}

fn sqlite_default(table: &str, col: &str) -> &'static str {
    match (table, col) {
        ("portal_user", "is_admin") => " default false",
        ("portal_user", "is_active") => " default true",
        ("portal_user", "reset_password") => " default false",
        ("portal_user", "email") => " default ''",
        _ => "",
    }
}

/// Create missing tables and add missing columns. Never drops or alters.
pub async fn sync_schema(db: &Db, ents: &[Entity]) -> Result<()> {
    if db.is_sqlite() {
        for e in ents {
            let mut cols = vec!["id integer primary key autoincrement".to_string(), "version bigint not null default 0".to_string()];
            for f in &e.fields {
                match f.kind {
                    K::Multi(_, join, own, other) => {
                        db.ddl(&format!("create table if not exists \"{}\" (\"{}\" bigint not null, \"{}\" bigint not null)", join, own, other)).await?;
                    }
                    k => {
                        let t = if e.table == "file_link" && f.name == "path" { "text" } else { sqltype(k).unwrap() };
                        let def = sqlite_default(e.table, f.name);
                        cols.push(format!("\"{}\" {} null{}", f.name, t, def));
                    }
                }
            }
            db.ddl(&format!("create table if not exists \"{}\" ({})", e.table, cols.join(", "))).await?;
            // Add any columns introduced since the table was first created.
            let existing: Vec<String> = db.table_columns(e.table).await?.into_iter().map(|(c, _)| c.to_ascii_lowercase()).collect();
            for f in &e.fields {
                if matches!(f.kind, K::Multi(..)) {
                    continue;
                }
                if !existing.iter().any(|c| c == &f.name.to_ascii_lowercase()) {
                    let t = if e.table == "file_link" && f.name == "path" { "text" } else { sqltype(f.kind).unwrap() };
                    let def = sqlite_default(e.table, f.name);
                    db.ddl(&format!("alter table \"{}\" add column \"{}\" {} null{}", e.table, f.name, t, def)).await?;
                }
            }
        }
        db.ddl(
            "create table if not exists portal_session (id varchar(80) primary key, data text, updated timestamp default (datetime('now', 'localtime')));
             create index if not exists ix_user_role_user on user_role (user_id);
             create index if not exists ix_ptf_tracker on portal_tracker_field (tracker_id);
             create index if not exists ix_pts_tracker on portal_tracker_status (tracker_id);
             create index if not exists ix_ptt_tracker on portal_tracker_transition (tracker_id);
             create index if not exists ix_ptr_tracker on portal_tracker_role (tracker_id);
             create index if not exists ix_tracker_ms on portal_tracker (module, slug);
             create index if not exists ix_filelink_ms on file_link (module, slug);
             create index if not exists ix_audit_date on portal_audit_trail (date);",
        )
        .await?;
        db.ddl(&format!("create index if not exists ix_{t}_ms on \"{t}\" (module, slug)", t = crate::db::page_t())).await?;
        db.ddl("create index if not exists ix_portal_tree_node_tree on portal_tree_node (tree_id, lft)").await?;
        return Ok(());
    }

    // On a Grails portal's database new tables take their ids from the same
    // hibernate_sequence its tables use, so rows copied across keep their ids
    // and rows added later never collide with either side's.
    let hib = db.first("select 1 as x from pg_sequences where sequencename='hibernate_sequence'", &crate::db::no_params()).await?.is_some();
    let id_col = if hib { "id bigint primary key default nextval('hibernate_sequence')" } else { "id bigserial primary key" };
    for e in ents {
        db.ddl(&format!("create table if not exists \"{}\" ({}, version bigint not null default 0)", e.table, id_col)).await?;
        for f in &e.fields {
            match f.kind {
                K::Multi(_, join, own, other) => {
                    db.ddl(&format!("create table if not exists \"{}\" (\"{}\" bigint not null, \"{}\" bigint not null)", join, own, other)).await?;
                }
                k => {
                    let t = sqltype(k).unwrap();
                    db.ddl(&format!("alter table \"{}\" add column if not exists \"{}\" {} null", e.table, f.name, t)).await?;
                }
            }
        }
    }
    // Tables created by the Grails portal (Hibernate) have no default for id - ids
    // come from hibernate_sequence - and a NOT NULL version without a default.
    // Give them defaults so both applications can insert into the same database:
    // ids from the same shared sequence, so they never collide.
    for e in ents {
        let p = crate::row! {"t"=>e.table};
        let col = db.first("select column_default, is_nullable from information_schema.columns where table_schema=current_schema() and table_name=:t and column_name='id'", &p).await?;
        if let Some(c) = col {
            if c.get("column_default").map(|v| v.is_null()).unwrap_or(true) {
                if hib {
                    db.ddl(&format!("alter table \"{}\" alter column id set default nextval('hibernate_sequence')", e.table)).await?;
                } else {
                    let seq = format!("{}_id_seq", e.table);
                    db.ddl(&format!(
                        "create sequence if not exists \"{s}\" owned by \"{t}\".id; select setval('\"{s}\"', greatest((select coalesce(max(id),0) from \"{t}\"), 1)); alter table \"{t}\" alter column id set default nextval('\"{s}\"')",
                        s = seq, t = e.table
                    ))
                    .await?;
                }
            }
        }
        let v = db.first("select column_default from information_schema.columns where table_schema=current_schema() and table_name=:t and column_name='version'", &p).await?;
        if v.map(|c| c.get("column_default").map(|x| x.is_null()).unwrap_or(true)).unwrap_or(false) {
            db.ddl(&format!("alter table \"{}\" alter column version set default 0", e.table)).await?;
        }
    }
    db.ddl(
        "create table if not exists portal_session (id varchar(80) primary key, data text, updated timestamp default now());
         alter table portal_user alter column is_admin set default false;
         alter table portal_user alter column is_active set default true;
         alter table portal_user alter column reset_password set default false;
         alter table portal_user alter column email set default '';
         create index if not exists ix_user_role_user on user_role (user_id);
         create index if not exists ix_ptf_tracker on portal_tracker_field (tracker_id);
         create index if not exists ix_pts_tracker on portal_tracker_status (tracker_id);
         create index if not exists ix_ptt_tracker on portal_tracker_transition (tracker_id);
         create index if not exists ix_ptr_tracker on portal_tracker_role (tracker_id);
         create index if not exists ix_tracker_ms on portal_tracker (module, slug);
         create index if not exists ix_filelink_ms on file_link (module, slug);
         create index if not exists ix_audit_date on portal_audit_trail (date);
         alter table file_link alter column path type text;
         alter table portal_tracker alter column condition_q type text;",
    )
    .await?;
    db.ddl(&format!("create index if not exists ix_{t}_ms on \"{t}\" (module, slug)", t = crate::db::page_t())).await?;
    db.ddl("create index if not exists ix_portal_tree_node_tree on portal_tree_node (tree_id, lft)").await?;
    Ok(())
}
