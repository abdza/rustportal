//! Outgoing e-mail + `/portalEmail/*` admin screens (matching g6ikram's
//! `PortalEmailController` and `views/portalEmail/*.gsp`). Every message is
//! recorded in `portal_email` first, then delivered over SMTP; failures stay
//! queued for the scheduler or manual `/portalEmail/run` to retry.

use crate::db::{self, b, i, lit, s, Row};
use crate::row;
use crate::web::{self, enc, esc, now_str, Ctx, St};
use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use axum::response::Response;
use lettre::message::{header::ContentType, Mailbox};
use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
use serde_json::{json, Value};

fn boxes(list: &str) -> Vec<Mailbox> {
    list.split([',', ';']).map(|x| x.trim()).filter(|x| !x.is_empty()).filter_map(|x| x.parse::<Mailbox>().ok()).collect()
}

async fn deliver(st: &St, e: &Row) -> Result<()> {
    let cfg = &st.cfg.mail;
    if cfg.disabled {
        return Err(anyhow!("mail is disabled in the configuration"));
    }
    let tos = boxes(&s(e, "emailto"));
    if tos.is_empty() {
        return Err(anyhow!("no valid recipient"));
    }
    let from_addr = web::setting_str(&st.db, "portal.emailfrom")
        .await
        .filter(|x| !x.trim().is_empty())
        .unwrap_or_else(|| cfg.from.clone());
    let mut b = Message::builder()
        .from(from_addr.parse::<Mailbox>().map_err(|e| anyhow!("bad from address: {}", e))?)
        .subject(s(e, "title"));
    for t in tos {
        b = b.to(t);
    }
    for c in boxes(&s(e, "emailcc")) {
        b = b.cc(c);
    }
    let msg = b.header(ContentType::TEXT_HTML).body(s(e, "body"))?;
    let mut tb = if cfg.starttls || cfg.port == 587 {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&cfg.host)?.port(cfg.port)
    } else if cfg.port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&cfg.host)?.port(cfg.port)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&cfg.host).port(cfg.port)
    };
    if !cfg.username.is_empty() {
        tb = tb.credentials(lettre::transport::smtp::authentication::Credentials::new(cfg.username.clone(), cfg.password.clone()));
    }
    tb.build().send(msg).await?;
    Ok(())
}

fn spawn_telegram_mirror(st: &St, to: &str, cc: &str, subject: &str, body: &str) {
    if st.cfg.run_telegram.unwrap_or(st.cfg.run_scheduler) {
        let db = st.db.clone();
        let all_rcpts = if cc.trim().is_empty() { to.to_string() } else { format!("{},{}", to, cc) };
        let subj = subject.to_string();
        let html = body.to_string();
        tokio::spawn(async move {
            crate::telegram::notify_emails(&db, &all_rcpts, &subj, &html).await;
        });
    }
}

pub async fn send_queued(st: &St, id: i64) -> Result<()> {
    let e = st.db.get("portal_email", id).await?.ok_or_else(|| anyhow!("email not found"))?;
    match deliver(st, &e).await {
        Ok(()) => {
            st.db.update("portal_email", id, &row! {"email_sent"=>true,"time_sent"=>now_str(),"error"=>None::<String>}).await?;
            Ok(())
        }
        Err(err) => {
            let _ = st.db.update("portal_email", id, &row! {"email_sent"=>false,"error"=>err.to_string()}).await;
            Err(err)
        }
    }
}

/// Record and send. Returns the portal_email id; delivery errors are logged, not raised.
pub async fn send(st: &St, to: &str, cc: &str, subject: &str, body: &str, module: &str) -> Result<i64> {
    if to.trim().is_empty() {
        return Err(anyhow!("send_email: no recipient"));
    }
    let id = st
        .db
        .insert("portal_email", &row! {"emailto"=>to,"emailcc"=>cc,"title"=>subject,"body"=>body,"module"=>module,"delivery_time"=>now_str(),"email_sent"=>false})
        .await?;
    if let Err(e) = send_queued(st, id).await {
        eprintln!("[mail] delivery of #{} failed: {}", id, e);
    }
    // Mirror to linked Telegram accounts asynchronously (never blocks or fails email send)
    spawn_telegram_mirror(st, to, cc, subject, body);
    Ok(id)
}

/// Retry messages that are due and not sent yet (called by the scheduler).
pub async fn flush(st: &St) {
    let rows = st
        .db
        .rows("select id from portal_email where email_sent is not true and (delivery_time is null or delivery_time <= now()) and (delivery_time is null or delivery_time > now() - interval '2 days') order by id limit 50", &Row::new())
        .await
        .unwrap_or_default();
    for r in rows {
        if let Some(id) = i(&r, "id") {
            let _ = send_queued(st, id).await;
        }
    }
}

// ---------------------------------------------------------------------------
// /portalEmail/* web controller (matches g6ikram PortalEmailController & GSPs)
// ---------------------------------------------------------------------------

fn fmt_dt(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    if let Some(dt) = crate::script::parse_dt(t) {
        return dt.format("%Y-%m-%d %H:%M:%S").to_string();
    }
    let norm = t.replace('T', " ");
    let base = norm.split('.').next().unwrap_or(&norm).trim_end_matches('Z');
    let base = if base.len() > 19 { &base[..19] } else { base };
    match crate::script::parse_dt(base) {
        Some(dt) => dt.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => t.to_string(),
    }
}

fn dt_input_val(raw: &str) -> String {
    let t = raw.trim();
    if t.is_empty() {
        return String::new();
    }
    let f = fmt_dt(t);
    f.replace(' ', "T").chars().take(16).collect()
}

fn fmt_bool(v: Option<&Value>) -> &'static str {
    match v {
        Some(Value::Bool(true)) => "True",
        Some(Value::Bool(false)) => "False",
        Some(Value::Number(n)) if n.as_i64() == Some(1) => "True",
        Some(Value::Number(n)) if n.as_i64() == Some(0) => "False",
        Some(Value::String(x)) if x.eq_ignore_ascii_case("true") || x == "1" => "True",
        Some(Value::String(x)) if x.eq_ignore_ascii_case("false") || x == "0" => "False",
        _ => "",
    }
}

/// Match g6ikram's `views/portalEmail/body/_displayWrapper.gsp`: `${raw(value?.replace("\r","<br/>"))}`.
fn render_email_body(raw: &str) -> String {
    raw.replace('\r', "<br/>")
}

fn page(nav: &str, kind: &str, title: &str, flash: &str, body: &str) -> String {
    let msg = if flash.is_empty() {
        String::new()
    } else {
        format!("                    <div class=\"message\" role=\"status\">{}</div>\n", esc(flash))
    };
    format!(
        "\n    <div id=\"content\" role=\"main\">\n        <div class=\"container\">\n            <section class=\"row\">\n                <a href=\"#{kind}-portalEmail\" class=\"skip\" tabindex=\"-1\">Skip to content&hellip;</a>\n                <div class=\"nav\" role=\"navigation\">\n                    <ul>\n                        <li><a class=\"home\" href=\"/\">Home</a></li>\n{nav}                    </ul>\n                </div>\n            </section>\n            <section class=\"row\">\n                <div id=\"{kind}-portalEmail\" class=\"col-12 content scaffold-{kind}\" role=\"main\">\n                    <h1>{}</h1>\n{msg}{body}\n                </div>\n            </section>\n        </div>\n    </div>\n",
        esc(title)
    )
}

fn li(class: &str, href: &str, label: &str) -> String {
    format!("                        <li><a class=\"{class}\" href=\"{href}\">{}</a></li>\n", esc(label))
}

async fn use_vim_editor(ctx: &Ctx) -> bool {
    let Some(u) = &ctx.user else { return false };
    let staffid = s(u, "userid");
    if staffid.is_empty() {
        return false;
    }
    let v = web::setting_str(ctx.db(), &format!("{}_editor", staffid)).await;
    matches!(v.as_deref().map(|x| x.trim().to_ascii_lowercase()).as_deref(), Some("vim" | "vi"))
}

fn email_ace_js(vim_mode: bool) -> String {
    let vim_script = if vim_mode {
        "\n<script src=\"/static/libs/ace_editor/js/keybinding-vim.js\"></script>"
    } else {
        ""
    };
    format!(
        r#"<link href="/static/libs/ace_editor/css/theme/github.css" rel="stylesheet"/>
<style type="text/css" media="screen">
  #outerbody {{ width: 68%; height: 500px; position: relative; display: inline-block; vertical-align: top; }}
  #bodydiv {{ position: absolute; top: 0; right: 0; bottom: 0; left: 0; border: 1px solid #ccc; }}
  .ace_editor, .ace_editor * {{
    font-family: "Monaco", "Menlo", "Ubuntu Mono", "Droid Sans Mono", "Consolas", monospace !important;
    font-size: 12px !important;
    font-weight: 400 !important;
    letter-spacing: 0 !important;
  }}
  form .ace_editor.fullScreen {{
    height: auto; width: auto; border: 0; margin: 0;
    position: fixed !important; top: 0; bottom: 0; left: 0; right: 0; z-index: 1000000;
  }}
  body.fullScreen {{ overflow: hidden; transform: none !important; }}
</style>
<script src="/static/libs/ace_editor/js/ace.js"></script>
<script src="/static/libs/ace_editor/js/theme-github.js"></script>
<script src="/static/libs/ace_editor/js/mode-html.js"></script>{vim_script}
<script>
$(function(){{
  if (typeof ace === 'undefined') {{
    $('#body').show();
    $('#outerbody').hide();
    return;
  }}
  ace.config.set('basePath', '/static/libs/ace_editor/js');
  var ta = document.getElementById('body');
  var ed = ace.edit('bodydiv');
  ed.setTheme('ace/theme/github');
  ed.session.setMode('ace/mode/html');
  ed.session.setValue(ta.value, -1);
  ed.session.on('change', function(){{ ta.value = ed.getValue(); }});
  var vimMode = {vim};
  if (vimMode) {{
    ed.setKeyboardHandler('ace/keyboard/vim');
    ace.config.loadModule('ace/keyboard/vim', function(module) {{
      var VimApi = (module && module.CodeMirror && module.CodeMirror.Vim) || (module && module.Vim);
      if (!VimApi) return;
      VimApi.defineEx('write', 'w', function() {{
        ta.value = ed.getValue();
        var btn = document.getElementById('updatebutton') || document.getElementById('create');
        if (btn) btn.click();
      }});
    }});
  }}
  ed.commands.addCommand({{
    name: 'Toggle Fullscreen',
    bindKey: 'F11',
    exec: function(editor) {{
      $('body').toggleClass('fullScreen');
      $('#bodydiv.ace_editor').toggleClass('fullScreen');
      editor.setAutoScrollEditorIntoView(true);
      editor.resize();
    }}
  }});
  ed.commands.addCommand({{
    name: 'Save Email',
    bindKey: {{ win: 'Ctrl-S', mac: 'Command-S' }},
    exec: function() {{
      ta.value = ed.getValue();
      var btn = document.getElementById('updatebutton') || document.getElementById('create');
      if (btn) btn.click();
    }}
  }});
  $('.content form').on('submit', function(){{ ta.value = ed.getValue(); }});
}});
</script>"#,
        vim_script = vim_script,
        vim = if vim_mode { "true" } else { "false" }
    )
}

async fn allowed_modules(ctx: &Ctx) -> Vec<String> {
    let Some(u) = &ctx.user else { return vec![] };
    if ctx.is_admin() {
        return vec![];
    }
    web::admin_list(ctx.db(), u).await
}

async fn can_manage_email(ctx: &Ctx, r: &Row) -> bool {
    if ctx.is_admin() {
        return true;
    }
    let m = s(r, "module");
    if m.is_empty() {
        return false;
    }
    allowed_modules(ctx).await.contains(&m)
}

pub async fn controller(ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let id = id.unwrap_or(0);
    match action {
        "" | "index" | "list" => index_action(ctx).await,
        "run" => run_action(ctx).await,
        "show" => show_action(ctx, id).await,
        "create" => create_action(ctx).await,
        "save" => save_action(ctx).await,
        "edit" => edit_action(ctx, id).await,
        "update" => update_action(ctx, id).await,
        "delete" => delete_action(ctx, id).await,
        "resend" => resend_action(ctx, id).await,
        _ => index_action(ctx).await,
    }
}

async fn index_action(ctx: Ctx) -> Response {
    let db = ctx.db();
    let is_admin = ctx.is_admin();
    let my_mods = allowed_modules(&ctx).await;
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this list").await;
    }

    let q = ctx.p("q");
    let m = ctx.p("module");
    let mut cond: Vec<String> = vec![];
    if !m.is_empty() && m != "All" {
        if !is_admin && !my_mods.contains(&m) {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this module").await;
        }
        cond.push(format!("\"module\"={}", lit(&json!(m))));
    } else if !is_admin {
        cond.push(format!("\"module\" in {}", lit(&json!(my_mods))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!(
            "(title ilike {l} or \"module\" ilike {l} or emailto ilike {l} or emailcc ilike {l} or body ilike {l})",
            l = like
        ));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "title" => format!("title {order_dir}, id desc"),
        "module" => format!("\"module\" {order_dir}, id desc"),
        "emailto" => format!("emailto {order_dir}, id desc"),
        "emailcc" => format!("emailcc {order_dir}, id desc"),
        "body" => format!("body {order_dir}, id desc"),
        "deliveryTime" | "delivery_time" => format!("delivery_time {order_dir}, id desc"),
        "emailSent" | "email_sent" => format!("email_sent {order_dir}, id desc"),
        "timeSent" | "time_sent" => format!("time_sent {order_dir}, id desc"),
        _ => "delivery_time desc, id desc".to_string(),
    };

    let total = db.count(&format!("select count(*) from portal_email{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(&format!("select * from portal_email{wh} order by {order_sql} limit {max} offset {offset}"), &db::no_params())
        .await
        .unwrap_or_default();

    const COLS: &[(&str, &str)] = &[
        ("title", "Title"),
        ("module", "Module"),
        ("emailto", "Emailto"),
        ("emailcc", "Emailcc"),
        ("body", "Body"),
        ("delivery_time", "Delivery Time"),
        ("email_sent", "Email Sent"),
        ("time_sent", "Time Sent"),
    ];
    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col
            || (*col == "delivery_time" && sort_p == "deliveryTime")
            || (*col == "email_sent" && sort_p == "emailSent")
            || (*col == "time_sent" && sort_p == "timeSent");
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        thead.push_str(&format!(
            "                <th class=\"{cls}\"><a href=\"/portalEmail/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{lbl}</a></th>\n"
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let eid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let title_txt = s(r, "title");
        let title_lbl = if title_txt.trim().is_empty() { format!("#{}", eid) } else { esc(&title_txt) };
        let body_html = render_email_body(&s(r, "body"));
        tbody.push_str(&format!(
            "            <tr class=\"{row_cls}\">\n                <td><a href=\"/portalEmail/show/{eid}\">{title_lbl}</a>&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td><div style=\"max-height:180px;overflow:auto;\">{body_html}</div>&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n                <td>{}&nbsp;</td>\n            </tr>\n",
            esc(&s(r, "module")),
            esc(&s(r, "emailto")),
            esc(&s(r, "emailcc")),
            esc(&fmt_dt(&s(r, "delivery_time"))),
            fmt_bool(r.get("email_sent")),
            esc(&fmt_dt(&s(r, "time_sent"))),
        ));
    }

    let pag = if total > max {
        format!("\n                    <div class=\"pagination\">\n                        {}\n                    </div>\n", crate::tags::paginate("/portalEmail/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };

    let mod_hidden = if !m.is_empty() && m != "All" {
        format!("<input type=\"hidden\" name=\"module\" value=\"{}\"/>", esc(&m))
    } else {
        String::new()
    };

    let body = format!(
        r#"<style>
@media only screen and (max-width: 760px), (min-device-width: 768px) and (max-device-width: 1024px) {{
  td:nth-of-type(1):before {{ content: "Title"; }}
  td:nth-of-type(2):before {{ content: "Module"; }}
  td:nth-of-type(3):before {{ content: "EmailTo"; }}
  td:nth-of-type(4):before {{ content: "EmailCC"; }}
  td:nth-of-type(5):before {{ content: "Body"; }}
  td:nth-of-type(6):before {{ content: "DeliveryTime"; }}
  td:nth-of-type(7):before {{ content: "EmailSent"; }}
  td:nth-of-type(8):before {{ content: "TimeSent"; }}
}}
</style>
                    <form action="/portalEmail/index" method="get">
                        {mod_hidden}
                        <fieldset class="form">
                            <div class="fieldcontain" id="listsearch">
                                <label>Search:</label><input type="text" name="q" id="q" value="{q}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
                            </div>
                        </fieldset>
                    </form>
                    <table class="responsive">
                        <thead>
                            <tr>
{thead}                            </tr>
                        </thead>
                        <tbody>
{tbody}                        </tbody>
                    </table>{pag}"#,
        q = esc(&q),
    );

    let create_href = if !m.is_empty() && m != "All" {
        format!("/portalEmail/create?module={}", enc(&m))
    } else {
        "/portalEmail/create".to_string()
    };
    let nav = format!(
        "{}{}",
        li("create", &create_href, "New PortalEmail"),
        li("create", "/portalEmail/run", "Send Emails"),
    );
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "list", "PortalEmail List", &flash, &body);
    ctx.html("PortalEmail List", h).await
}

async fn run_action(ctx: Ctx) -> Response {
    let is_admin = ctx.is_admin();
    let my_mods = allowed_modules(&ctx).await;
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to run the emailer").await;
    }
    let scope = if is_admin {
        String::new()
    } else {
        format!(" and \"module\" in {}", lit(&json!(my_mods)))
    };
    let rows = ctx
        .db()
        .rows(
            &format!(
                "select id, emailto, emailcc, title, body from portal_email where email_sent is not true and (delivery_time is null or delivery_time <= now()){scope} order by id asc limit 200"
            ),
            &db::no_params(),
        )
        .await
        .unwrap_or_default();

    let mut sent = 0usize;
    let mut failed = 0usize;
    for r in &rows {
        if let Some(eid) = i(r, "id") {
            match send_queued(&ctx.st, eid).await {
                Ok(()) => sent += 1,
                Err(_) => failed += 1,
            }
            spawn_telegram_mirror(&ctx.st, &s(r, "emailto"), &s(r, "emailcc"), &s(r, "title"), &s(r, "body"));
        }
    }

    let body = format!(
        "<p>Processed {} queued email(s): <strong>{}</strong> sent, <strong>{}</strong> failed.</p><p><a href=\"/portalEmail/index\">Back to PortalEmail List</a></p>",
        rows.len(),
        sent,
        failed
    );
    let nav = format!(
        "{}{}",
        li("list", "/portalEmail/index", "PortalEmail List"),
        li("create", "/portalEmail/create", "New PortalEmail"),
    );
    let h = page(&nav, "list", "Running emailer", "", &body);
    ctx.html("PortalEmail List", h).await
}

async fn show_action(mut ctx: Ctx, id: i64) -> Response {
    let Ok(Some(r)) = ctx.db().get("portal_email", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalEmail not found").await;
    };
    if !can_manage_email(&ctx, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this email").await;
    }

    let mod_name = s(&r, "module");
    let mod_cell = match crate::admin::module_url(ctx.db(), &mod_name).await {
        Some(u) => format!("<a href=\"{u}\">{}</a>", esc(&mod_name)),
        None => esc(&mod_name),
    };
    let body_html = render_email_body(&s(&r, "body"));
    let err_txt = s(&r, "error");
    let err_row = if err_txt.is_empty() {
        String::new()
    } else {
        format!(
            "    <li class=\"fieldcontain\">\n        <span id=\"error-label\" class=\"property-label\">Error</span>\n        <div class=\"property-value\" aria-labelledby=\"error-label\"><span style=\"color:#b02a37;\">{}</span></div>\n    </li>\n",
            esc(&err_txt)
        )
    };

    let csrf = ctx.csrf_field();
    let content = format!(
        r#"<ol class="property-list portalEmail">
    <li class="fieldcontain">
        <span id="title-label" class="property-label">Title</span>
        <div class="property-value" aria-labelledby="title-label">{title}</div>
    </li>
    <li class="fieldcontain">
        <span id="module-label" class="property-label">Module</span>
        <div class="property-value" aria-labelledby="module-label">{mod_cell}</div>
    </li>
    <li class="fieldcontain">
        <span id="emailto-label" class="property-label">Emailto</span>
        <div class="property-value" aria-labelledby="emailto-label">{emailto}</div>
    </li>
    <li class="fieldcontain">
        <span id="emailcc-label" class="property-label">Emailcc</span>
        <div class="property-value" aria-labelledby="emailcc-label">{emailcc}</div>
    </li>
    <li class="fieldcontain">
        <span id="body-label" class="property-label">Body</span>
        <div class="property-value" aria-labelledby="body-label">{body_html}</div>
    </li>
    <li class="fieldcontain">
        <span id="deliveryTime-label" class="property-label">Delivery Time</span>
        <div class="property-value" aria-labelledby="deliveryTime-label">{delivery_time}</div>
    </li>
    <li class="fieldcontain">
        <span id="emailSent-label" class="property-label">Email Sent</span>
        <div class="property-value" aria-labelledby="emailSent-label">{email_sent}</div>
    </li>
    <li class="fieldcontain">
        <span id="timeSent-label" class="property-label">Time Sent</span>
        <div class="property-value" aria-labelledby="timeSent-label">{time_sent}</div>
    </li>
{err_row}</ol>
                    <form action="/portalEmail/delete/{id}" method="post">{csrf}
                        <fieldset class="buttons">
                            <a href="/portalEmail/edit/{id}" class="edit">Edit</a>
                            <button type="submit" formaction="/portalEmail/resend/{id}" class="save">Send Now</button>
                            <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
                        </fieldset>
                    </form>"#,
        title = esc(&s(&r, "title")),
        emailto = esc(&s(&r, "emailto")),
        emailcc = esc(&s(&r, "emailcc")),
        delivery_time = esc(&fmt_dt(&s(&r, "delivery_time"))),
        email_sent = fmt_bool(r.get("email_sent")),
        time_sent = esc(&fmt_dt(&s(&r, "time_sent"))),
    );

    let nav = format!(
        "{}{}",
        li("list", "/portalEmail/index", "PortalEmail List"),
        li("create", "/portalEmail/create", "New PortalEmail"),
    );
    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, "show", "Show PortalEmail", &flash, &content);
    ctx.html("Show PortalEmail", h).await
}

async fn render_form(mut ctx: Ctx, r: &Row, id: Option<i64>, errors: &[String]) -> Response {
    let is_admin = ctx.is_admin();
    let mut mods: Vec<String> = ctx
        .db()
        .rows("select name from portal_module order by name", &db::no_params())
        .await
        .unwrap_or_default()
        .iter()
        .map(|m| s(m, "name"))
        .collect();
    if !is_admin {
        let mine = allowed_modules(&ctx).await;
        mods.retain(|m| mine.contains(m));
    }
    let cur_mod = s(r, "module");
    if !cur_mod.is_empty() && !mods.contains(&cur_mod) {
        mods.push(cur_mod.clone());
    }

    let mut mod_opts = String::from("<option value=\"\"></option>");
    for m in &mods {
        let sel = if *m == cur_mod { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(m), esc(m)));
    }
    let mod_cls = if is_admin { "module-select-tags" } else { "module-select" };

    let err_html = if errors.is_empty() {
        String::new()
    } else {
        let items: String = errors.iter().map(|e| format!("<li>{}</li>", esc(e))).collect();
        format!("<ul class=\"errors\" role=\"alert\">{items}</ul>\n")
    };

    let (action_url, kind, title, nav, submit_btn) = match id {
        Some(eid) => (
            format!("/portalEmail/update/{eid}"),
            "edit",
            "Edit PortalEmail",
            format!("{}{}", li("list", "/portalEmail/index", "PortalEmail List"), li("create", "/portalEmail/create", "New PortalEmail")),
            "<input id=\"updatebutton\" class=\"save\" type=\"submit\" value=\"Update\" />",
        ),
        None => (
            "/portalEmail/save".to_string(),
            "create",
            "Create PortalEmail",
            li("list", "/portalEmail/index", "PortalEmail List"),
            "<input type=\"submit\" name=\"create\" class=\"save\" value=\"Create\" id=\"create\" />",
        ),
    };

    let csrf = ctx.csrf_field();
    let vim = use_vim_editor(&ctx).await;
    let ace_js = email_ace_js(vim);
    let select2_tags = if is_admin {
        "<script>$(function(){ if ($.fn && $.fn.select2) { $('select.module-select-tags').select2({ width: '40%', tags: true, allowClear: true, placeholder: '- choose a module -' }); } });</script>"
    } else {
        ""
    };

    let checked = if b(r, "email_sent") { " checked=\"checked\"" } else { "" };
    let body = format!(
        r#"{err_html}                    <form action="{action_url}" method="post">{csrf}
                        <fieldset class="form">
                            <div class="fieldcontain required">
                                <label for="title">Title<span class="required-indicator">*</span></label>
                                <input type="text" name="title" value="{title_val}" required="" id="title" />
                            </div>
                            <div class="fieldcontain">
                                <label for="module">Module</label>
                                <select name="module" id="module" class="{mod_cls}">{mod_opts}</select>
                            </div>
                            <div class="fieldcontain required">
                                <label for="emailto">Emailto<span class="required-indicator">*</span></label>
                                <textarea name="emailto" required="" id="emailto" rows="4" cols="40">{emailto_val}</textarea>
                            </div>
                            <div class="fieldcontain">
                                <label for="emailcc">Emailcc</label>
                                <textarea name="emailcc" id="emailcc" rows="3" cols="40">{emailcc_val}</textarea>
                            </div>
                            <div class="fieldcontain">
                                <label for="body">Body</label>
                                <div id="outerbody"><div id="bodydiv"></div></div>
                                <textarea name="body" id="body" style="display:none">{body_val}</textarea>
                            </div>
                            <div class="fieldcontain">
                                <label for="delivery_time">Delivery Time</label>
                                <input type="datetime-local" name="delivery_time" value="{delivery_val}" id="delivery_time" />
                            </div>
                            <div class="fieldcontain">
                                <label for="email_sent">Email Sent</label>
                                <input type="hidden" name="_email_sent" />
                                <input type="checkbox" name="email_sent" id="email_sent"{checked} />
                            </div>
                            <div class="fieldcontain">
                                <label for="time_sent">Time Sent</label>
                                <input type="datetime-local" name="time_sent" value="{timesent_val}" id="time_sent" />
                            </div>
                        </fieldset>
                        <fieldset class="buttons">
                            {submit_btn}
                        </fieldset>
                    </form>
{form_js}
{select2_tags}
{ace_js}"#,
        title_val = esc(&s(r, "title")),
        emailto_val = esc(&s(r, "emailto")),
        emailcc_val = esc(&s(r, "emailcc")),
        body_val = esc(&s(r, "body")),
        delivery_val = esc(&dt_input_val(&s(r, "delivery_time"))),
        timesent_val = esc(&dt_input_val(&s(r, "time_sent"))),
        form_js = crate::tags::form_js(),
    );

    let flash = s(&ctx.sess, "flash");
    let h = page(&nav, kind, title, &flash, &body);
    ctx.html(title, h).await
}

fn posted_email(ctx: &Ctx) -> Row {
    let opt_str = |k: &str| {
        let v = ctx.p(k);
        if v.trim().is_empty() { Value::Null } else { json!(v.trim()) }
    };
    let opt_text = |k: &str| {
        let v = ctx.p(k);
        if v.is_empty() { Value::Null } else { json!(v.replace("\r\n", "\n")) }
    };
    let opt_dt = |k1: &str, k2: &str| {
        let raw = {
            let a = ctx.p(k1);
            if a.trim().is_empty() { ctx.p(k2) } else { a }
        };
        if raw.trim().is_empty() { Value::Null } else { json!(raw.trim().replace('T', " ")) }
    };
    let sent_raw = if ctx.params.contains_key("email_sent") {
        let v = ctx.p("email_sent");
        v != "false" && v != "0"
    } else if ctx.params.contains_key("emailSent") {
        let v = ctx.p("emailSent");
        v != "false" && v != "0"
    } else {
        false
    };
    row! {
        "title" => opt_str("title"),
        "module" => opt_str("module"),
        "emailto" => opt_text("emailto"),
        "emailcc" => opt_text("emailcc"),
        "body" => opt_text("body"),
        "delivery_time" => opt_dt("delivery_time", "deliveryTime"),
        "email_sent" => json!(sent_raw),
        "time_sent" => opt_dt("time_sent", "timeSent"),
    }
}

async fn create_action(ctx: Ctx) -> Response {
    if !ctx.is_admin() && allowed_modules(&ctx).await.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to create emails").await;
    }
    let mut r = Row::new();
    let m = ctx.p("module");
    if !m.is_empty() {
        r.insert("module".into(), json!(m));
    }
    r.insert("delivery_time".into(), json!(now_str()));
    r.insert("email_sent".into(), json!(false));
    render_form(ctx, &r, None, &[]).await
}

async fn save_action(mut ctx: Ctx) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect("/portalEmail/create").await;
    }
    let d = posted_email(&ctx);
    let m = s(&d, "module");
    if !ctx.is_admin() {
        let mine = allowed_modules(&ctx).await;
        if m.is_empty() || !mine.contains(&m) {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to create emails in this module").await;
        }
    }
    let mut errs = vec![];
    if s(&d, "title").trim().is_empty() {
        errs.push("Title cannot be blank".into());
    }
    if s(&d, "emailto").trim().is_empty() {
        errs.push("Emailto cannot be blank".into());
    }
    if !errs.is_empty() {
        return render_form(ctx, &d, None, &errs).await;
    }
    match ctx.db().insert("portal_email", &d).await {
        Ok(eid) => {
            ctx.flash(&format!("PortalEmail {} created", eid));
            ctx.redirect(&format!("/portalEmail/show/{}", eid)).await
        }
        Err(e) => render_form(ctx, &d, None, &[format!("Could not save: {}", e)]).await,
    }
}

async fn edit_action(ctx: Ctx, id: i64) -> Response {
    let Ok(Some(r)) = ctx.db().get("portal_email", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalEmail not found").await;
    };
    if !can_manage_email(&ctx, &r).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this email").await;
    }
    render_form(ctx, &r, Some(id), &[]).await
}

async fn update_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalEmail/edit/{}", id)).await;
    }
    let Ok(Some(existing)) = ctx.db().get("portal_email", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalEmail not found").await;
    };
    if !can_manage_email(&ctx, &existing).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to edit this email").await;
    }
    let d = posted_email(&ctx);
    let m = s(&d, "module");
    if !ctx.is_admin() {
        let mine = allowed_modules(&ctx).await;
        if m.is_empty() || !mine.contains(&m) {
            return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to move this email to that module").await;
        }
    }
    let mut errs = vec![];
    if s(&d, "title").trim().is_empty() {
        errs.push("Title cannot be blank".into());
    }
    if s(&d, "emailto").trim().is_empty() {
        errs.push("Emailto cannot be blank".into());
    }
    if !errs.is_empty() {
        return render_form(ctx, &d, Some(id), &errs).await;
    }
    match ctx.db().update("portal_email", id, &d).await {
        Ok(_) => {
            ctx.flash(&format!("PortalEmail {} updated", id));
            ctx.redirect(&format!("/portalEmail/show/{}", id)).await
        }
        Err(e) => render_form(ctx, &d, Some(id), &[format!("Could not update: {}", e)]).await,
    }
}

async fn delete_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let Ok(Some(existing)) = ctx.db().get("portal_email", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalEmail not found").await;
    };
    if !can_manage_email(&ctx, &existing).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to delete this email").await;
    }
    let _ = ctx.db().delete("portal_email", id).await;
    ctx.flash(&format!("PortalEmail {} deleted", id));
    ctx.redirect("/portalEmail/index").await
}

async fn resend_action(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let Ok(Some(existing)) = ctx.db().get("portal_email", id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "PortalEmail not found").await;
    };
    if !can_manage_email(&ctx, &existing).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    match send_queued(&ctx.st, id).await {
        Ok(()) => {
            spawn_telegram_mirror(&ctx.st, &s(&existing, "emailto"), &s(&existing, "emailcc"), &s(&existing, "title"), &s(&existing, "body"));
            ctx.flash("Email sent");
        }
        Err(e) => ctx.flash(&format!("Sending failed: {}", e)),
    }
    ctx.redirect(&format!("/portalEmail/show/{}", id)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_email_body_replaces_carriage_returns_like_g6ikram() {
        assert_eq!(render_email_body("Line1\rLine2\r\n<p>Hi</p>"), "Line1<br/>Line2<br/>\n<p>Hi</p>");
    }

    #[test]
    fn email_datetime_and_bool_formatters() {
        assert_eq!(fmt_dt("2026-11-22T07:55:10.123Z"), "2026-11-22 07:55:10");
        assert_eq!(dt_input_val("2026-11-22 07:55:10"), "2026-11-22T07:55");
        assert_eq!(fmt_bool(Some(&json!(true))), "True");
        assert_eq!(fmt_bool(Some(&json!(false))), "False");
        assert_eq!(fmt_bool(None), "");
    }

    #[test]
    fn email_ace_js_switches_vim_mode() {
        let normal = email_ace_js(false);
        assert!(normal.contains("ace/mode/html"));
        assert!(!normal.contains("keybinding-vim.js"));
        let vim = email_ace_js(true);
        assert!(vim.contains("keybinding-vim.js"));
        assert!(vim.contains("ace/keyboard/vim"));
    }
}
