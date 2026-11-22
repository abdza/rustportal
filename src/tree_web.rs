//! The tree admin screens, laid out like the Grails portal's: the tree page links
//! to its root node, and the node page is a jsTree of the whole tree with the
//! selected node's details, its role holders (add / change / remove in a dialog),
//! drag-and-drop moving, and New Node / Edit / Delete / Edit Domain buttons.

use crate::db::{self, b, i, lit, s, Row};
use crate::row;
use crate::tree::{export_tree, import_tree, rebuild, ImportStats};
use crate::web::{self, enc, esc, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;

async fn tree_of(ctx: &Ctx, tree_id: i64) -> Option<Row> {
    ctx.db().get("portal_tree", tree_id).await.ok().flatten()
}

/// A node and its tree, if the user may administer the tree's module.
async fn node_and_tree(ctx: &Ctx, id: i64) -> Result<(Row, Row), (StatusCode, &'static str)> {
    let node = ctx.db().get("portal_tree_node", id).await.ok().flatten().ok_or((StatusCode::NOT_FOUND, "Tree node not found"))?;
    let tree = tree_of(ctx, i(&node, "tree_id").unwrap_or(0)).await.ok_or((StatusCode::NOT_FOUND, "Tree not found"))?;
    if !ctx.can_admin(&s(&tree, "module")).await {
        return Err((StatusCode::FORBIDDEN, "You are not authorized to manage this tree"));
    }
    Ok((node, tree))
}

/// The tree's root: its root_id, else its first top-level node.
async fn root_id(ctx: &Ctx, tree: &Row) -> Option<i64> {
    if let Some(r) = i(tree, "root_id") {
        return Some(r);
    }
    ctx.db().first("select id from portal_tree_node where tree_id=:t and parent_id is null order by lft nulls last, id limit 1", &row! {"t"=>i(tree, "id")}).await.ok().flatten().and_then(|r| i(&r, "id"))
}

/// Where the Grails portal goes after any change: the root's page with the node selected.
async fn back_to(ctx: &Ctx, tree: &Row, node: i64) -> String {
    match root_id(ctx, tree).await {
        Some(r) => format!("/portalTreeNode/show/{}?nodeid={}", r, node),
        None => format!("/portalTree/show/{}", i(tree, "id").unwrap_or(0)),
    }
}

fn page(nav: &str, kind: &str, name: &str, title: &str, flash: &str, body: &str) -> String {
    format!(
        "\n    <div id=\"content\" role=\"main\">\n        <div class=\"container\">\n            <section class=\"row\">\n                <div class=\"nav\" role=\"navigation\">\n                    <ul>\n                        <li><a class=\"home\" href=\"/\">Home</a></li>\n{nav}                    </ul>\n                </div>\n            </section>\n            <section class=\"row\">\n                <div id=\"{kind}-{name}\" class=\"col-12 content scaffold-{kind}\" role=\"main\">\n                    <h1>{title}</h1>\n{msg}{body}\n                </div>\n            </section>\n        </div>\n    </div>\n",
        msg = if flash.is_empty() { String::new() } else { format!("                    <div class=\"message\" role=\"status\">{}</div>\n", esc(flash)) },
        title = esc(title)
    )
}

fn li(class: &str, href: &str, label: &str) -> String {
    format!("                        <li><a class=\"{}\" href=\"{}\">{}</a></li>\n", class, href, label)
}

fn field(name: &str, label: &str, v: &Row, textarea: bool) -> String {
    let val = esc(&s(v, name));
    if textarea {
        format!("<div class=\"fieldcontain\"><label for=\"{n}\">{l}</label><textarea name=\"{n}\" id=\"{n}\" rows=\"5\" cols=\"40\">{val}</textarea></div>\n", n = name, l = label)
    } else {
        format!("<div class=\"fieldcontain\"><label for=\"{n}\">{l}</label><input type=\"text\" name=\"{n}\" value=\"{val}\" id=\"{n}\" /></div>\n", n = name, l = label)
    }
}

/// The node form's fields, as the Grails portal shows them (create leaves out the domain and disabled).
fn node_fields(v: &Row, edit: bool) -> String {
    let mut o = String::new();
    o.push_str(&field("name", "Name", v, false));
    o.push_str(&field("slug", "Slug", v, false));
    o.push_str(&field("data", "Data", v, true));
    o.push_str(&field("mainrole", "Mainrole", v, false));
    o.push_str(&field("hiderole", "Hiderole", v, false));
    if edit {
        o.push_str(&field("domain", "Domain", v, false));
        o.push_str(&field("domainid", "Domainid", v, false));
        o.push_str(&format!("<div class=\"fieldcontain\"><label for=\"disabled\">Disabled</label><input type=\"hidden\" name=\"_disabled\" /><input type=\"checkbox\" name=\"disabled\" id=\"disabled\" {}/></div>\n", if b(v, "disabled") { "checked=\"checked\" " } else { "" }));
    }
    o
}

fn posted_node(ctx: &Ctx, edit: bool) -> Row {
    let opt = |k: &str| { let v = ctx.p(k); if v.trim().is_empty() { Value::Null } else { json!(v) } };
    let mut d = row! {"name"=>opt("name"),"slug"=>opt("slug"),"data"=>opt("data"),"mainrole"=>opt("mainrole"),"hiderole"=>opt("hiderole")};
    if edit {
        d.insert("domain".into(), opt("domain"));
        d.insert("domainid".into(), ctx.p("domainid").trim().parse::<i64>().map(|x| json!(x)).unwrap_or(Value::Null));
        d.insert("disabled".into(), json!(ctx.params.contains_key("disabled")));
    }
    d
}

/// The nested list jsTree reads (the Grails portal's treenodes tag): a node with
/// exactly one role holder shows it next to its name.
fn nodes_li(nodes: &[Row], users: &[Row], n: &Row, o: &mut String) {
    let id = i(n, "id");
    let us: Vec<&Row> = users.iter().filter(|u| i(u, "node_id") == id).collect();
    let who = if us.len() == 1 { format!(" - {}: {} ({})", esc(&s(us[0], "role")), esc(&s(us[0], "name")), esc(&s(us[0], "userid"))) } else { String::new() };
    o.push_str(&format!("<li class='jstree-drop' id='{}'><a href='#'>{}{}</a>", id.unwrap_or(0), esc(&s(n, "name")), who));
    let kids: Vec<&Row> = nodes.iter().filter(|c| i(c, "parent_id") == id).collect();
    if !kids.is_empty() {
        o.push_str("<ul>");
        for c in kids {
            nodes_li(nodes, users, c, o);
        }
        o.push_str("</ul>");
    }
    o.push_str("</li>");
}

const NODE_SCRIPT: &str = r##"
<script src="/static/libs/jstree/jstree.min.js"></script>
<script>
$(function() {
var tree = $('div.jstree');
tree.jstree({
  "core" : {
    "animation" : 0,
    "check_callback" : function(op) { return op !== 'move_node' || $('#enablemove').prop('checked'); },
    "themes" : { "stripes" : true }
  },
  "search" : { "show_only_matches" : false },
  "plugins" : [ "search", "dnd" ]
});

window.inituserform = function() {
  $('#usernodeid').val("");
  $('#useruserid').val("");
  $('#nodeaction').val('new');
  $('#delete_user_button').hide();
  $('#role').val("");
  $('#user').empty();
  $('#add_user_button').text('Add User');
};

window.userclick = function(el) {
  $('#usernodeid').val($(el).data('node_id'));
  $('#useruserid').val($(el).data('user_id'));
  $('#nodeaction').val('edit');
  $('#delete_user_button').show();
  $('#role').val($(el).data('staff_role'));
  $('#user').empty();
  $('#user').append(new Option($(el).data('staff_name'), $(el).data('user_id'), true, true)).trigger('change');
  $('#add_user_button').text('Update User');
  new bootstrap.Modal(document.getElementById("userformdialog"), {}).show();
  return false;
};

$('#delete_user_button').on("click", function() {
  if (confirm('Delete user?')) {
    $('#nodeaction').val('delete');
    $('#role').prop('required', false);
    $('#user_form').submit();
  }
  return false;
});

tree.on("move_node.jstree", function(e, m) {
  if ($('#enablemove').prop('checked')) {
    $.post('/portalTreeNode/movenode', { o: m.node.id, r: m.parent, p: m.position })
      .fail(function() { alert('The node could not be moved there'); location.reload(); });
  }
}).on("select_node.jstree", function(e, data) {
  var id = data.node.id;
  $('a.createnode').attr('href', '/portalTreeNode/create?parentid=' + id);
  $('a.editnode').attr('href', '/portalTreeNode/edit/' + id);
  $('a.editdomain').attr('href', '/portalTreeNode/edit_domain/' + id);
  $('#deleteform').attr('action', '/portalTreeNode/delete/' + id);
  fetch('/portalTreeNode/json/' + id).then(function(r) { return r.json(); }).then(function(data) {
    var n = data.node;
    $('#delete_user_button').hide();
    $('#nodeid').text(n.id);
    $('#hidenodeid').val(n.id);
    $('#nodename').text(n.name || '');
    $('#nodeslug').text(n.slug || '');
    $('#nodepath').text(n.path || '');
    $('#nodelftrgt').text(n.lft + ' - ' + n.rgt);
    $('#nodedomain').text(n.domain || '');
    $('#nodedomainid').text(n.domainid == null ? '' : n.domainid);
    $('#nodedata').text(n.data || '');
    $('#add_user_button').text('Add User');
    $('#nodeaction').val('new');
    var count = $("<span data-bs-toggle='collapse' data-bs-target='#userlist' style='cursor:pointer'></span>").text(n.users.length + ' users');
    var ul = $("<ul class='collapse' id='userlist'></ul>");
    n.users.forEach(function(u) {
      var a = $("<a href='#' onclick='return userclick(this)'></a>").text(u.role + ' : ' + u.name)
        .attr({ 'data-staff_name': u.name, 'data-staff_role': u.role, 'data-node_id': u.id, 'data-user_id': u.user_id });
      var item = $('<li></li>').append(a).append(' : ').append($("<a></a>").attr('href', '/user/show/' + u.user_id).text('Info'));
      if (data.canswitch) item.append(' : ').append($("<a></a>").attr('href', '/user/switchuser/' + encodeURIComponent(u.staff_id)).text('Switch'));
      ul.append(item);
    });
    $('#nodeusers').empty().append(count).append(ul);
  });
}).on("ready.jstree", function() {
  var q = $('#q').val();
  if (q) tree.jstree(true).search(q);
  var sel = SELECTED_NODE;
  if (sel) { tree.jstree(true).select_node(String(sel)); var el = document.getElementById(String(sel)); if (el) el.scrollIntoView({ block: 'center' }); }
});

$('.user-selector').select2({
  dropdownParent: $("#userformdialog"),
  width: '60%',
  ajax: {
    url: "/user/api_list",
    dataType: 'json',
    delay: 250,
    data: function(p) { return { q: p.term, id: $('#useruserid').val(), page: p.page }; },
    processResults: function(data) { return { results: data.users.map(function(u) { return { id: u.id, text: u.name }; }) }; }
  }
});
});
</script>
"##;

/// /portalTreeNode/show/{id}
async fn show(mut ctx: Ctx, id: i64) -> Response {
    let (node, tree) = match node_and_tree(&ctx, id).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    let tid = i(&tree, "id").unwrap_or(0);
    let nodes = ctx.db().rows("select id, parent_id, name from portal_tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tid}).await.unwrap_or_default();
    let users = ctx.db().rows("select tnu.node_id, tnu.role, u.name, u.userid from portal_tree_node_user tnu join portal_tree_node n on n.id=tnu.node_id join portal_user u on u.id=tnu.user_id where n.tree_id=:t", &row! {"t"=>tid}).await.unwrap_or_default();
    let mut list = String::new();
    nodes_li(&nodes, &users, &node, &mut list);
    let selected = ctx.pi("nodeid").unwrap_or(id);
    let q = ctx.p("q");
    let csrf = ctx.csrf_field();
    let flash = ctx.take_flash();
    let msg = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let prop = |id: &str, label: &str| format!("<li class='fieldcontain'><span class='property-label'>{}</span><div class='property-value' id='{}'></div></li>", label, id);
    let h = format!(
        r##"<link href="/static/libs/jstree/themes/default/style.min.css" rel="stylesheet">
<style>#treebody {{ max-height: 400px; overflow-y: auto; }}</style>
<div id="userformdialog" class="modal" tabindex="-1">
  <div class="modal-dialog">
    <div class="modal-content">
      <div class="modal-header">
        <h5 class="modal-title">Node User</h5>
        <button type="button" class="btn-close" data-bs-dismiss="modal" aria-label="Close"></button>
      </div>
    <form name='user_form' id='user_form' action='/portalTreeNode/user_form' method="POST">{csrf}
        <input type='hidden' id='hidenodeid' name='hidenodeid' value='{selected}' />
        <input type='hidden' id='usernodeid' name='usernodeid' />
        <input type='hidden' id='useruserid' name='useruserid' />
        <input type='hidden' id='nodeaction' name='nodeaction' value='new' />
      <div class="modal-body">
            <fieldset class="form">
                <div class="fieldcontain required">
                <label for="user">User <span class="required-indicator">*</span></label>
                <select id='user' name='user' class='user-selector' style='width: 60%;'></select>
                </div>
            </fieldset>
            <fieldset class="form">
                <div class="fieldcontain required">
                <label for="role">Role <span class="required-indicator">*</span></label>
                <input type="text" name="role" value="" required="" id="role">
                </div>
            </fieldset>
      </div>
      <div class="modal-footer">
        <button type="submit" class="btn btn-warning" id='delete_user_button' style='display:none'>Delete User</button>
        <button type="submit" class="btn btn-primary" id='add_user_button'>Add User</button>
        <button type="button" class="btn btn-secondary" data-bs-dismiss="modal">Close</button>
      </div>
    </form>
    </div>
  </div>
</div>

    <div id="content" role="main">
        <div class="container">
            {msg}
            <div class='card'>
                <div class='card-header'>
                    <form name='deleteform' id='deleteform' action='/portalTreeNode/delete/{selected}' method="POST">{csrf}
                    <a class="create createnode btn btn-outline-secondary" href="/portalTreeNode/create?parentid={selected}"><i class="bi bi-file-plus"></i> New Node</a>
                    <a class="edit editnode btn btn-outline-secondary" href="/portalTreeNode/edit/{selected}"><i class="bi bi-file-check"></i> Edit</a>
                    <button type="button" class="delete deletenode btn btn-outline-secondary" onclick='if(confirm("Confirm delete?")){{ $("#deleteform").submit() }}'><i class="bi bi-file-excel"></i> Delete</button>
                    <a class="create editdomain btn btn-outline-secondary" href="/portalTreeNode/edit_domain/{selected}"><i class="bi bi-bookmark-check"></i> Edit Domain</a>
                    <button type="button" class="adduser btn btn-outline-secondary" data-bs-toggle="modal" data-bs-target="#userformdialog" onclick="inituserform();return true;"><i class="bi bi-person-plus"></i> Add User</button>
                    Move: <input type="checkbox" id="enablemove"/>
                    <a class="btn btn-link" href="/portalTree/show/{tid}">{tree_name}</a>
                    </form>
                </div>
                <div class='card-body'>
                <ol class='property-list'>
                    {p_id}{p_name}{p_slug}{p_path}{p_lftrgt}{p_domain}{p_domainid}{p_data}{p_users}
                </ol>
                </div>
            </div>
            <div class='card'>
                <div class='card-header'>
                    <form class='horizontal-form' method='get' action='/portalTreeNode/show/{id}'>
                        <fieldset class="form">
                            <div class="fieldcontain required">
                            <label for="search">Search</label>
                            <input type="text" name="q" value="{q}" id="q">
                            <button type="submit" class="btn btn-primary">Search</button>
                            </div>
                        </fieldset>
                    </form>
                </div>
                <div id='treebody' class='card-body'>
                    <div id='tree{id}' class='jstree'><ul class='jstree-drop'>{list}</ul></div>
                </div>
            </div>
        </div>
    </div>
{script}"##,
        tree_name = esc(&s(&tree, "name")),
        q = esc(&q),
        p_id = prop("nodeid", "Id"), p_name = prop("nodename", "Name"), p_slug = prop("nodeslug", "Slug"), p_path = prop("nodepath", "Full Path"),
        p_lftrgt = prop("nodelftrgt", "Lft/Rgt"), p_domain = prop("nodedomain", "Domain"), p_domainid = prop("nodedomainid", "Domain ID"),
        p_data = prop("nodedata", "Data"), p_users = prop("nodeusers", "Users"),
        script = NODE_SCRIPT.replace("SELECTED_NODE", &selected.to_string()),
    );
    ctx.html("Show PortalTreeNode", h).await
}

/// /portalTreeNode/json/{id}: what the node page shows for the selected node.
async fn node_json(ctx: Ctx, id: i64) -> Response {
    let (node, _tree) = match node_and_tree(&ctx, id).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    let db = ctx.db();
    let users = db.rows("select tnu.id, tnu.user_id, tnu.role, u.name, u.userid from portal_tree_node_user tnu join portal_user u on u.id=tnu.user_id where tnu.node_id=:n order by tnu.role, u.name", &row! {"n"=>id}).await.unwrap_or_default();
    // ancestors, root first (the Grails portal's getfullpath)
    let path = db.rows("select name from portal_tree_node where tree_id=:t and lft<:l and rgt>:r order by lft", &row! {"t"=>i(&node, "tree_id"),"l"=>i(&node, "lft").unwrap_or(0),"r"=>i(&node, "rgt").unwrap_or(0)}).await.unwrap_or_default();
    let mut names: Vec<String> = path.iter().map(|r| s(r, "name")).collect();
    names.push(s(&node, "name"));
    let canswitch = ctx.is_admin() && !ctx.sess.contains_key("adminlink");
    let v = json!({
        "canswitch": canswitch,
        "node": {
            "id": id, "name": s(&node, "name"), "slug": s(&node, "slug"), "path": names.join(" / "), "domain": s(&node, "domain"),
            "domainid": node.get("domainid").cloned().unwrap_or(Value::Null), "data": s(&node, "data"),
            "lft": node.get("lft").cloned().unwrap_or(Value::Null), "rgt": node.get("rgt").cloned().unwrap_or(Value::Null),
            "parent": node.get("parent_id").cloned().unwrap_or(Value::Null),
            "users": users.iter().map(|u| json!({"id": i(u, "id"), "user_id": i(u, "user_id"), "name": format!("{} ({})", s(u, "name"), s(u, "userid")), "role": s(u, "role"), "staff_id": s(u, "userid")})).collect::<Vec<_>>(),
        }
    });
    ctx.json(v).await
}

/// POST /portalTreeNode/movenode o=node r=new parent p=position among its children
async fn movenode(ctx: Ctx) -> Response {
    let (Some(o), Some(r)) = (ctx.pi("o"), ctx.pi("r")) else { return ctx.status(StatusCode::BAD_REQUEST, "A node can only be moved under another node").await };
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST only").await;
    }
    let (node, tree) = match node_and_tree(&ctx, o).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    let tid = i(&tree, "id").unwrap_or(0);
    let db = ctx.db();
    let all = db.rows("select id, parent_id from portal_tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tid}).await.unwrap_or_default();
    if !all.iter().any(|n| i(n, "id") == Some(r)) {
        return ctx.status(StatusCode::BAD_REQUEST, "Nodes can only be moved within their own tree").await;
    }
    // the new parent must not be the node itself or below it
    let mut cur = Some(r);
    while let Some(c) = cur {
        if c == o {
            return ctx.status(StatusCode::BAD_REQUEST, "A node cannot be moved under itself").await;
        }
        cur = all.iter().find(|n| i(n, "id") == Some(c)).and_then(|n| i(n, "parent_id"));
    }
    let mut sibs: Vec<i64> = all.iter().filter(|n| i(n, "parent_id") == Some(r) && i(n, "id") != Some(o)).filter_map(|n| i(n, "id")).collect();
    let pos = ctx.pi("p").unwrap_or(sibs.len() as i64).clamp(0, sibs.len() as i64) as usize;
    sibs.insert(pos, o);
    let _ = db.exec("update portal_tree_node set parent_id=:r where id=:o", &row! {"r"=>r,"o"=>o}).await;
    // rebuild() keeps siblings in lft order, so number them in their new order first
    for (ix, sid) in sibs.iter().enumerate() {
        let _ = db.exec("update portal_tree_node set lft=:l where id=:id", &row! {"l"=>ix as i64,"id"=>*sid}).await;
    }
    let _ = rebuild(db, tid).await;
    crate::cache::MENUS.clear();
    let _ = node;
    ctx.json(json!({"ok": true})).await
}

/// POST /portalTreeNode/user_form: add, change or remove a role holder of a node.
async fn user_form(mut ctx: Ctx) -> Response {
    let nid = ctx.pi("hidenodeid").unwrap_or(0);
    let (_node, tree) = match node_and_tree(&ctx, nid).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST only").await;
    }
    let db = ctx.db().clone();
    let role = ctx.p("role").trim().to_string();
    let user = ctx.pi("user");
    // the row being changed must belong to this node
    let tnu = match ctx.pi("usernodeid") {
        Some(x) => db.get("portal_tree_node_user", x).await.ok().flatten().filter(|r| i(r, "node_id") == Some(nid)),
        None => None,
    };
    match ctx.p("nodeaction").as_str() {
        "new" => match user {
            Some(u) if !role.is_empty() => {
                let _ = db.insert("portal_tree_node_user", &row! {"node_id"=>nid,"user_id"=>u,"role"=>role}).await;
                ctx.flash("User added to the node");
            }
            _ => ctx.flash("Choose a user and enter a role"),
        },
        "edit" => match (tnu, user) {
            (Some(t), Some(u)) if !role.is_empty() => {
                let tid = i(&t, "id").unwrap_or(0);
                let _ = db.update("portal_tree_node_user", tid, &row! {"user_id"=>u,"role"=>role}).await;
                // the row now belongs to someone else: the previous holder stops acting as it
                if i(&t, "user_id") != Some(u) {
                    let _ = db.exec("update portal_user set roletargetid=null where roletargetid=:t and id<>:u", &row! {"t"=>tid,"u"=>u}).await;
                }
                ctx.flash("Node user updated");
            }
            _ => ctx.flash("Choose a user and enter a role"),
        },
        "delete" => {
            if let Some(t) = tnu {
                let tid = i(&t, "id").unwrap_or(0);
                let _ = db.exec("update portal_user set roletargetid=null where roletargetid=:t", &row! {"t"=>tid}).await;
                let _ = db.delete("portal_tree_node_user", tid).await;
                ctx.flash("User removed from the node");
            }
        }
        _ => {}
    }
    let to = back_to(&ctx, &tree, nid).await;
    ctx.redirect(&to).await
}

/// /portalTreeNode/edit_domain/{id}: open the record the node points at.
async fn edit_domain(mut ctx: Ctx, id: i64) -> Response {
    let (node, tree) = match node_and_tree(&ctx, id).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    let domain = s(&node, "domain");
    let did = i(&node, "domainid");
    if domain.trim().is_empty() || did.is_none() {
        ctx.flash("This node is not linked to a record (it has no domain and domain id)");
        let to = back_to(&ctx, &tree, id).await;
        return ctx.redirect(&to).await;
    }
    let dtype = domain.rsplit('.').next().unwrap_or(&domain).to_string();
    // the tracker_objects setting maps a domain name to "module.tracker_slug"
    let map = match crate::web::setting(ctx.db(), "tracker_objects").await {
        Some(Value::Object(m)) => Some(m),
        Some(Value::String(t)) => serde_json::from_str::<Value>(&t).ok().and_then(|v| v.as_object().cloned()),
        _ => None,
    };
    if let Some(target) = map.and_then(|m| m.get(&dtype).map(db::vs)) {
        if let Some((m, sl)) = target.split_once('.') {
            return ctx.redirect(&format!("/portalTracker/display_data?module={}&slug={}&id={}", crate::web::enc(m), crate::web::enc(sl), did.unwrap_or(0))).await;
        }
    }
    let mut c = dtype.chars();
    let controller = match c.next() {
        Some(f) => f.to_lowercase().collect::<String>() + c.as_str(),
        None => String::new(),
    };
    ctx.redirect(&format!("/{}/edit/{}", controller, did.unwrap_or(0))).await
}

/// /portalTreeNode/create?parentid= and its POST to /portalTreeNode/save
async fn create(mut ctx: Ctx) -> Response {
    let parent = ctx.pi("parentid").unwrap_or(0);
    let (pnode, tree) = match node_and_tree(&ctx, parent).await {
        Ok(x) => x,
        Err((_, _)) => {
            ctx.flash("New nodes are added under an existing node: open the tree and choose New Node");
            return ctx.redirect("/portalTree/index").await;
        }
    };
    if ctx.method == "POST" {
        let mut d = posted_node(&ctx, false);
        if s(&d, "name").is_empty() {
            ctx.flash("Name is required");
        } else {
            d.insert("tree_id".into(), json!(i(&tree, "id")));
            d.insert("parent_id".into(), json!(parent));
            d.insert("disabled".into(), json!(false));
            match ctx.db().insert("portal_tree_node", &d).await {
                Ok(nid) => {
                    let _ = rebuild(ctx.db(), i(&tree, "id").unwrap_or(0)).await;
                    crate::cache::MENUS.clear();
                    ctx.flash(&format!("PortalTreeNode {} created", nid));
                    let to = back_to(&ctx, &tree, nid).await;
                    return ctx.redirect(&to).await;
                }
                Err(e) => ctx.flash(&format!("Could not save: {}", e)),
            }
        }
    }
    let v = if ctx.method == "POST" { posted_node(&ctx, false) } else { Row::new() };
    let csrf = ctx.csrf_field();
    let flash = ctx.take_flash();
    let body = format!(
        "                    <form action=\"/portalTreeNode/save\" method=\"post\">{csrf}\n                        <input type='hidden' name='parentid' value='{parent}'/>\n                        <p>Under: <b>{pname}</b></p>\n                        <fieldset class=\"form\">\n{f}                        </fieldset>\n                        <fieldset class=\"buttons\">\n                            <input type=\"submit\" name=\"create\" class=\"save\" value=\"Create\" id=\"create\" />\n                        </fieldset>\n                    </form>",
        pname = esc(&s(&pnode, "name")), f = node_fields(&v, false)
    );
    let back = back_to(&ctx, &tree, parent).await;
    let h = page(&li("list", &back, "PortalTreeNode List"), "create", "portalTreeNode", "Create PortalTreeNode", &flash, &body);
    ctx.html("Create PortalTreeNode", h).await
}

/// /portalTreeNode/edit/{id} and its POST to /portalTreeNode/update/{id}
async fn edit(mut ctx: Ctx, id: i64) -> Response {
    let (node, tree) = match node_and_tree(&ctx, id).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    if ctx.method == "POST" {
        let d = posted_node(&ctx, true);
        if s(&d, "name").is_empty() {
            ctx.flash("Name is required");
        } else {
            match ctx.db().update("portal_tree_node", id, &d).await {
                Ok(_) => {
                    crate::cache::MENUS.clear();
                    ctx.flash(&format!("PortalTreeNode {} updated", id));
                    let to = back_to(&ctx, &tree, id).await;
                    return ctx.redirect(&to).await;
                }
                Err(e) => ctx.flash(&format!("Could not save: {}", e)),
            }
        }
    }
    let v = if ctx.method == "POST" { posted_node(&ctx, true) } else { node };
    let csrf = ctx.csrf_field();
    let flash = ctx.take_flash();
    let body = format!(
        "                    <form action=\"/portalTreeNode/update/{id}\" method=\"post\">{csrf}\n                        <fieldset class=\"form\">\n{f}                        </fieldset>\n                        <fieldset class=\"buttons\">\n                            <input class=\"save\" type=\"submit\" value=\"Update\" />\n                        </fieldset>\n                    </form>",
        f = node_fields(&v, true)
    );
    let back = back_to(&ctx, &tree, id).await;
    let h = page(&li("list", &back, "PortalTreeNode List"), "edit", "portalTreeNode", "Edit PortalTreeNode", &flash, &body);
    ctx.html("Edit PortalTreeNode", h).await
}

/// POST /portalTreeNode/delete/{id}: the node, everything under it, and their role holders.
async fn delete(mut ctx: Ctx, id: i64) -> Response {
    let (_node, tree) = match node_and_tree(&ctx, id).await {
        Ok(x) => x,
        Err((c, m)) => return ctx.status(c, m).await,
    };
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST only").await;
    }
    let tid = i(&tree, "id").unwrap_or(0);
    let db = ctx.db().clone();
    let _ = crate::tree::delete_node_deep(&db, tid, id).await;
    crate::cache::MENUS.clear();
    ctx.flash(&format!("PortalTreeNode {} deleted", id));
    let tree = tree_of(&ctx, tid).await.unwrap_or(tree);
    let to = match root_id(&ctx, &tree).await {
        Some(r) => format!("/portalTreeNode/show/{}?nodeid={}", r, r),
        None => format!("/portalTree/show/{}", tid),
    };
    ctx.redirect(&to).await
}

/// /portalTreeNode/fixnodes: renumber lft/rgt of every tree the user manages.
async fn fixnodes(mut ctx: Ctx) -> Response {
    let trees = ctx.db().rows("select id, module from portal_tree order by id", &db::no_params()).await.unwrap_or_default();
    let mut n = 0;
    for t in &trees {
        if ctx.can_admin(&s(t, "module")).await {
            let _ = rebuild(ctx.db(), i(t, "id").unwrap_or(0)).await;
            n += 1;
        }
    }
    crate::cache::MENUS.clear();
    ctx.flash(&format!("Nodes renumbered in {} tree(s)", n));
    ctx.redirect("/portalTree/index").await
}

pub async fn node_action(ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    let id = id.unwrap_or(0);
    match action {
        "show" => show(ctx, id).await,
        "json" => node_json(ctx, id).await,
        "movenode" => movenode(ctx).await,
        "user_form" => user_form(ctx).await,
        "edit_domain" => edit_domain(ctx, id).await,
        "create" | "save" => create(ctx).await,
        "edit" | "update" => edit(ctx, id).await,
        "delete" => delete(ctx, id).await,
        "fixnodes" => fixnodes(ctx).await,
        _ => {
            // the tree's own page is where nodes are browsed
            ctx.redirect("/portalTree/index").await
        }
    }
}

fn fmt_tree_dt(raw: &str) -> String {
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

fn tree_import_dir(ctx: &Ctx) -> PathBuf {
    PathBuf::from(&ctx.st.cfg.module_dir).join("treeimports")
}

/// Simple colored unified diff HTML for tree import preview.
fn diff_lines_html(old_text: &str, new_text: &str) -> (String, bool) {
    let old_lines: Vec<&str> = if old_text.is_empty() { vec![] } else { old_text.lines().collect() };
    let new_lines: Vec<&str> = new_text.lines().collect();
    if old_lines == new_lines {
        return (String::new(), false);
    }
    let mut out = String::from("<pre style=\"max-height:460px;overflow:auto;border:1px solid #ddd;padding:10px;background:#fff;font-size:12px;\">");
    if old_lines.is_empty() {
        out.push_str("<span style=\"color:#555;\">--- /dev/null\n+++ b/tree.json\n</span>");
        for l in &new_lines {
            out.push_str(&format!("<span style=\"color:#1a7f37;background:#dafbe1;display:block;\">+{}</span>", esc(l)));
        }
    } else {
        out.push_str("<span style=\"color:#555;\">--- a/tree.json\n+++ b/tree.json\n</span>");
        let (m, n) = (old_lines.len(), new_lines.len());
        // LCS on reasonable sizes, else line-set fallback
        if m * n <= 2_000_000 {
            let mut dp = vec![vec![0u32; n + 1]; m + 1];
            for r in (0..m).rev() {
                for c in (0..n).rev() {
                    dp[r][c] = if old_lines[r] == new_lines[c] {
                        dp[r + 1][c + 1] + 1
                    } else {
                        dp[r + 1][c].max(dp[r][c + 1])
                    };
                }
            }
            let (mut r, mut c) = (0, 0);
            let mut ops: Vec<(char, &str)> = vec![];
            while r < m && c < n {
                if old_lines[r] == new_lines[c] {
                    ops.push((' ', old_lines[r]));
                    r += 1;
                    c += 1;
                } else if dp[r + 1][c] >= dp[r][c + 1] {
                    ops.push(('-', old_lines[r]));
                    r += 1;
                } else {
                    ops.push(('+', new_lines[c]));
                    c += 1;
                }
            }
            while r < m { ops.push(('-', old_lines[r])); r += 1; }
            while c < n { ops.push(('+', new_lines[c])); c += 1; }
            // Show changed lines + 3 lines of context
            let mut show = vec![false; ops.len()];
            for (idx, (k, _)) in ops.iter().enumerate() {
                if *k != ' ' {
                    let lo = idx.saturating_sub(3);
                    let hi = (idx + 3).min(ops.len().saturating_sub(1));
                    for s_i in lo..=hi {
                        show[s_i] = true;
                    }
                }
            }
            let mut skipped = false;
            for (idx, (k, line)) in ops.iter().enumerate() {
                if !show[idx] {
                    if !skipped {
                        out.push_str("<span style=\"color:#6e7781;display:block;\">@@ ... @@</span>");
                        skipped = true;
                    }
                    continue;
                }
                skipped = false;
                match k {
                    '-' => out.push_str(&format!("<span style=\"color:#cf222e;background:#ffebe9;display:block;\">-{}</span>", esc(line))),
                    '+' => out.push_str(&format!("<span style=\"color:#1a7f37;background:#dafbe1;display:block;\">+{}</span>", esc(line))),
                    _ => out.push_str(&format!("<span style=\"color:#333;display:block;\"> {}</span>", esc(line))),
                }
            }
        } else {
            for l in &old_lines {
                out.push_str(&format!("<span style=\"color:#cf222e;background:#ffebe9;display:block;\">-{}</span>", esc(l)));
            }
            for l in &new_lines {
                out.push_str(&format!("<span style=\"color:#1a7f37;background:#dafbe1;display:block;\">+{}</span>", esc(l)));
            }
        }
    }
    out.push_str("</pre>");
    (out, true)
}

fn normalize_tree_json(trees: &[Row], staff: bool) -> String {
    fn clean_node(n: &Value, staff: bool) -> Value {
        let Some(o) = n.as_object() else { return Value::Null };
        let kids: Vec<Value> = o.get("nodes").and_then(|v| v.as_array()).map(|a| a.iter().map(|c| clean_node(c, staff)).collect()).unwrap_or_default();
        let mut users = Value::Null;
        if staff {
            if let Some(u) = o.get("users").and_then(|v| v.as_array()) {
                let mut sorted = u.clone();
                sorted.sort_by(|a, b| {
                    let ka = (a.get("userid").and_then(|x| x.as_str()).unwrap_or(""), a.get("role").and_then(|x| x.as_str()).unwrap_or(""));
                    let kb = (b.get("userid").and_then(|x| x.as_str()).unwrap_or(""), b.get("role").and_then(|x| x.as_str()).unwrap_or(""));
                    ka.cmp(&kb)
                });
                users = Value::Array(sorted);
            } else {
                users = Value::Array(vec![]);
            }
        }
        json!({
            "name": o.get("name").cloned().unwrap_or(Value::Null),
            "slug": o.get("slug").cloned().unwrap_or(Value::Null),
            "data": o.get("data").cloned().unwrap_or(Value::Null),
            "domain": o.get("domain").cloned().unwrap_or(Value::Null),
            "mainrole": o.get("mainrole").cloned().unwrap_or(Value::Null),
            "hiderole": o.get("hiderole").cloned().unwrap_or(Value::Null),
            "disabled": o.get("disabled").cloned().unwrap_or(json!(false)),
            "users": users,
            "nodes": kids
        })
    }
    let arr: Vec<Value> = trees
        .iter()
        .map(|t| {
            let kids: Vec<Value> = t.get("nodes").and_then(|v| v.as_array()).map(|a| a.iter().map(|c| clean_node(c, staff)).collect()).unwrap_or_default();
            json!({
                "module": t.get("module").cloned().unwrap_or(Value::Null),
                "name": t.get("name").cloned().unwrap_or(Value::Null),
                "root": t.get("root").cloned().unwrap_or(Value::Null),
                "nodes": kids
            })
        })
        .collect();
    serde_json::to_string_pretty(&Value::Array(arr)).unwrap_or_default()
}

/// Main dispatcher for `/portalTree/*` matching g6ikram's `PortalTreeController` + `views/portalTree/*.gsp`.
pub async fn tree_action(mut ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match action {
        "index" => tree_index(ctx).await,
        "show" => match id {
            Some(tid) => tree_show(ctx, tid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Tree not found").await,
        },
        "create" => tree_create_or_edit(ctx, None).await,
        "save" => tree_save(ctx).await,
        "edit" => match id {
            Some(tid) => tree_create_or_edit(ctx, Some(tid)).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Tree not found").await,
        },
        "update" => match id {
            Some(tid) => tree_update(ctx, tid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Tree not found").await,
        },
        "delete" => match id {
            Some(tid) => tree_delete(ctx, tid).await,
            None => ctx.status(StatusCode::NOT_FOUND, "Tree not found").await,
        },
        "create_root" | "exporttree" => {
            let Some(tree) = tree_of(&ctx, id.unwrap_or(0)).await else { return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await };
            if !ctx.can_admin(&s(&tree, "module")).await {
                return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to manage this tree").await;
            }
            let tid = i(&tree, "id").unwrap_or(0);
            if action == "create_root" {
                if root_id(&ctx, &tree).await.is_none() {
                    if let Ok(r) = ctx.db().insert("portal_tree_node", &row! {"tree_id"=>tid,"name"=>s(&tree, "name"),"disabled"=>false}).await {
                        let _ = ctx.db().update("portal_tree", tid, &row! {"root_id"=>r}).await;
                        let _ = rebuild(ctx.db(), tid).await;
                    }
                }
                return ctx.redirect(&format!("/portalTree/show/{}", tid)).await;
            }
            let staff = ctx.params.contains_key("staff");
            match export_tree(ctx.db(), tid, staff).await {
                Ok(v) => {
                    let name: String = format!("tree_{}_{}", { let m = s(&tree, "module"); if m.is_empty() { "portal".to_string() } else { m } }, s(&tree, "name"))
                        .chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-') { c } else { '_' }).collect();
                    let body = serde_json::to_string_pretty(&Value::Array(vec![v])).unwrap_or_default().into_bytes();
                    ctx.bytes("application/json", Some(&format!("{}.json", name)), body, false).await
                }
                Err(e) => ctx.status(StatusCode::INTERNAL_SERVER_ERROR, &format!("Export failed: {}", e)).await,
            }
        }
        "importtreeform" => {
            let csrf = ctx.csrf_field();
            let flash = ctx.take_flash();
            let body = format!(r#"                    <p>
                        Upload a tree file exported from another server (or one tree taken out of a
                        module's <code>treelist.json</code>). The tree is matched by module and name:
                        an existing one is updated in place, a new one is created. Nothing is deleted -
                        nodes this server has that the file does not are left alone.
                    </p>
                    <form method="POST" action="/portalTree/importtree" enctype="multipart/form-data">{csrf}
                        <fieldset class="form">
                          <div class="fieldcontain">
                            <label for="fileupload">Tree File</label>
                            <input type="file" name="fileupload" id="fileupload" accept=".json" required/>
                          </div>
                          <div class="fieldcontain">
                            <label for="staff">Include role holders</label>
                            <input type="checkbox" name="staff" id="staff"/>
                            <span class="property-value">Staff assigned to each node, matched by staff id. Leave unticked to move the structure only.</span>
                          </div>
                        </fieldset>
                        <fieldset class="buttons">
                            <input type="submit" name="upload" class="save" value="Review Changes" />
                        </fieldset>
                    </form>"#);
            let h = page(&li("list", "/portalTree/index", "PortalTree List"), "create", "import-tree", "Import Tree", &flash, &body);
            ctx.html("Import Tree", h).await
        }
        "importtree" => {
            if ctx.method != "POST" {
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let Some(up) = ctx.files.remove("fileupload") else {
                ctx.flash("No file uploaded");
                return ctx.redirect("/portalTree/importtreeform").await;
            };
            let text = String::from_utf8_lossy(&up.read_all().await.unwrap_or_default()).to_string();
            let trees: Vec<Row> = match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(a)) => a.into_iter().filter_map(|v| v.as_object().cloned()).collect(),
                Ok(Value::Object(o)) => vec![o],
                Ok(_) => vec![],
                Err(e) => {
                    ctx.flash(&format!("Could not read that file as a tree export: {}", e));
                    return ctx.redirect("/portalTree/importtreeform").await;
                }
            };
            if trees.is_empty() || trees.iter().any(|t| s(t, "name").trim().is_empty()) {
                ctx.flash("That file does not look like a tree export (no tree name in it)");
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let mut denied = vec![];
            for t in &trees {
                let m = { let m = s(t, "module"); if m.is_empty() { "portal".to_string() } else { m } };
                if !ctx.can_admin(&m).await && !denied.contains(&m) {
                    denied.push(m);
                }
            }
            if !denied.is_empty() {
                ctx.flash(&format!("You need admin rights on module {} to import that tree", denied.join(", ")));
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let staff = ctx.params.contains_key("staff");
            let dir = tree_import_dir(&ctx);
            let _ = std::fs::create_dir_all(&dir);
            let stamp = chrono::Local::now().timestamp_millis();
            let uid_s: String = ctx
                .user
                .as_ref()
                .map(|u| s(u, "userid"))
                .unwrap_or_else(|| "anon".into())
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            let fname = format!("{}_{}.json", stamp, if uid_s.is_empty() { "anon" } else { &uid_s });
            if let Err(e) = tokio::fs::write(dir.join(&fname), text.as_bytes()).await {
                ctx.flash(&format!("Could not stage upload: {}", e));
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let staff_q = if staff { "&staff=on" } else { "" };
            ctx.redirect(&format!("/portalTree/importtreepreview?parked={}{}", enc(&fname), staff_q)).await
        }
        "importtreepreview" => {
            let parked_name = ctx.p("parked");
            if parked_name.is_empty() || parked_name.contains('/') || parked_name.contains('\\') || parked_name.contains("..") {
                ctx.flash("That upload is no longer available, please upload the file again");
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let parked_path = tree_import_dir(&ctx).join(&parked_name);
            let Ok(text) = tokio::fs::read_to_string(&parked_path).await else {
                ctx.flash("That upload is no longer available, please upload the file again");
                return ctx.redirect("/portalTree/importtreeform").await;
            };
            let trees: Vec<Row> = match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(a)) => a.into_iter().filter_map(|v| v.as_object().cloned()).collect(),
                Ok(Value::Object(o)) => vec![o],
                _ => vec![],
            };
            let staff = ctx.params.contains_key("staff");
            let mut list_items = String::new();
            let mut cur_exports: Vec<Row> = vec![];
            for t in &trees {
                let m = { let x = s(t, "module"); if x.is_empty() { "portal".into() } else { x } };
                if !ctx.can_admin(&m).await {
                    ctx.flash(&format!("You need admin rights on module {m} to import that tree"));
                    return ctx.redirect("/portalTree/importtreeform").await;
                }
                let nm = s(t, "name");
                let existing = ctx
                    .db()
                    .first("select id from portal_tree where \"module\"=:m and name=:n", &row! {"m"=>m.clone(),"n"=>nm.clone()})
                    .await
                    .ok()
                    .flatten();
                let known = existing.is_some();
                if let Some(ex) = existing {
                    if let Ok(Value::Object(obj)) = export_tree(ctx.db(), i(&ex, "id").unwrap_or(0), staff).await {
                        cur_exports.push(obj);
                    }
                }
                list_items.push_str(&format!(
                    "<li>{} / <strong>{}</strong> &mdash; {}</li>",
                    esc(&m),
                    esc(&nm),
                    if known { "already here, will be updated in place" } else { "not on this server yet, will be created" }
                ));
            }
            let old_json = if cur_exports.is_empty() { String::new() } else { normalize_tree_json(&cur_exports, staff) };
            let new_json = normalize_tree_json(&trees, staff);
            let (diff_html, has_diff) = diff_lines_html(&old_json, &new_json);
            let diff_block = if has_diff {
                diff_html
            } else {
                "<div class=\"message\" role=\"status\">No changes detected between this server's tree and the uploaded file.</div>".into()
            };
            let csrf = ctx.csrf_field();
            let flash = ctx.take_flash();
            let staff_hidden = if staff { "<input type=\"hidden\" name=\"staff\" value=\"on\"/>" } else { "" };
            let body = format!(
                "<p>Lines starting with <span style=\"color:#cf222e;\">-</span> show what this server holds now, \
                 lines starting with <span style=\"color:#1a7f37;\">+</span> show what the file brings. \
                 Importing role holders: <strong>{}</strong>.</p>\
                 <ul>{list_items}</ul>\
                 {diff_block}\
                 <form action=\"/portalTree/confirmimporttree\" method=\"post\">{csrf}\
                   <input type=\"hidden\" name=\"parked\" value=\"{}\"/>{staff_hidden}\
                   <fieldset class=\"buttons\">\
                     <a class=\"edit\" href=\"/portalTree/index\">Cancel</a>\
                     <input type=\"submit\" name=\"confirm\" class=\"save\" value=\"Import Tree\" onclick=\"return confirm('Import this tree?');\"/>\
                   </fieldset>\
                 </form>",
                if staff { "Yes" } else { "No" },
                esc(&parked_name)
            );
            let nav_extra = format!("{}{}", li("list", "/portalTree/index", "PortalTree List"), li("create", "/portalTree/importtreeform", "Upload another file"));
            let h = page(&nav_extra, "show", "import-tree-preview", "Import Tree Preview", &flash, &body);
            ctx.html("Import Tree Preview", h).await
        }
        "confirmimporttree" => {
            if ctx.method != "POST" {
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let parked_name = ctx.p("parked");
            if parked_name.is_empty() || parked_name.contains('/') || parked_name.contains('\\') || parked_name.contains("..") {
                ctx.flash("That upload is no longer available, please upload the file again");
                return ctx.redirect("/portalTree/importtreeform").await;
            }
            let parked_path = tree_import_dir(&ctx).join(&parked_name);
            let Ok(text) = tokio::fs::read_to_string(&parked_path).await else {
                ctx.flash("That upload is no longer available, please upload the file again");
                return ctx.redirect("/portalTree/importtreeform").await;
            };
            let trees: Vec<Row> = match serde_json::from_str::<Value>(&text) {
                Ok(Value::Array(a)) => a.into_iter().filter_map(|v| v.as_object().cloned()).collect(),
                Ok(Value::Object(o)) => vec![o],
                _ => vec![],
            };
            for t in &trees {
                let m = { let x = s(t, "module"); if x.is_empty() { "portal".into() } else { x } };
                if !ctx.can_admin(&m).await {
                    ctx.flash(&format!("You need admin rights on module {m} to import that tree"));
                    return ctx.redirect("/portalTree/importtreeform").await;
                }
            }
            let staff = ctx.params.contains_key("staff");
            let mut stats = ImportStats::default();
            let mut last = None;
            let mut errs = vec![];
            for t in &trees {
                match import_tree(ctx.db(), t, None, staff, &mut stats).await {
                    Ok(tid) => last = Some(tid),
                    Err(e) => errs.push(format!("{}: {}", s(t, "name"), e)),
                }
            }
            let _ = tokio::fs::remove_file(&parked_path).await;
            crate::cache::MENUS.clear();
            let mut msg = format!("Tree imported ({})", stats.summary());
            if !errs.is_empty() {
                msg.push_str(&format!(". Failed: {}", errs.join("; ")));
            }
            if !stats.users_skipped.is_empty() {
                msg.push_str(&format!(". Staff not found: {}", stats.users_skipped.iter().take(10).cloned().collect::<Vec<_>>().join(", ")));
            }
            ctx.flash(&msg);
            match last {
                Some(tid) => ctx.redirect(&format!("/portalTree/show/{}", tid)).await,
                None => ctx.redirect("/portalTree/index").await,
            }
        }
        "rebuild" => {
            if ctx.method != "POST" {
                return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
            }
            let tid = id.unwrap_or(0);
            let Some(tree) = tree_of(&ctx, tid).await else {
                return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await;
            };
            if !ctx.can_admin(&s(&tree, "module")).await {
                return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
            }
            let _ = rebuild(ctx.db(), tid).await;
            ctx.flash("Tree rebuilt");
            ctx.redirect(&format!("/portalTree/show/{}", tid)).await
        }
        _ => tree_index(ctx).await,
    }
}

async fn tree_index(ctx: Ctx) -> Response {
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let u = ctx.user.as_ref().unwrap();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, u).await };
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
        cond.push(format!("t.\"module\"={}", lit(&json!(m))));
    } else if !is_admin {
        cond.push(format!("t.\"module\" in {}", lit(&json!(my_mods))));
    }
    if !q.trim().is_empty() {
        let like = lit(&json!(format!("%{}%", q.trim())));
        cond.push(format!("(t.name ilike {l} or t.\"module\" ilike {l})", l = like));
    }
    let wh = if cond.is_empty() { String::new() } else { format!(" where {}", cond.join(" and ")) };
    let max = ctx.pi("max").unwrap_or(10).clamp(1, 500);
    let offset = ctx.pi("offset").unwrap_or(0).max(0);
    let sort_p = ctx.p("sort");
    let order_dir = if ctx.p("order") == "desc" { "desc" } else { "asc" };
    let order_sql = match sort_p.as_str() {
        "module" => format!("t.\"module\" {}, t.id asc", order_dir),
        "name" => format!("t.name {}, t.id asc", order_dir),
        "root" | "root_id" => format!("r.name {}, t.id asc", order_dir),
        "expire" => format!("t.expire {}, t.id asc", order_dir),
        "valid" => format!("t.valid {}, t.id asc", order_dir),
        _ => "t.\"module\" asc, t.name asc".to_string(),
    };

    let from_sql = "from portal_tree t left join portal_tree_node r on r.id=t.root_id";
    let total = db.count(&format!("select count(*) {from_sql}{wh}"), &db::no_params()).await.unwrap_or(0);
    let rows = db
        .rows(
            &format!("select t.*, r.name as root_name {from_sql}{wh} order by {order_sql} limit {max} offset {offset}"),
            &db::no_params(),
        )
        .await
        .unwrap_or_default();

    // Fallback root lookup for trees whose root_id is null but have a top-level node
    let mut fallback_roots: HashMap<i64, (i64, String)> = HashMap::new();
    for r in &rows {
        if i(r, "root_id").is_none() {
            if let Some(tid) = i(r, "id") {
                if let Ok(Some(rn)) = db.first("select id, name from portal_tree_node where tree_id=:t and parent_id is null order by lft nulls last, id limit 1", &row! {"t"=>tid}).await {
                    if let Some(nid) = i(&rn, "id") {
                        fallback_roots.insert(tid, (nid, s(&rn, "name")));
                    }
                }
            }
        }
    }

    let mut mods: Vec<String> = if is_admin {
        db.rows("select name from portal_module order by name", &db::no_params())
            .await
            .unwrap_or_default()
            .iter()
            .map(|r| s(r, "name"))
            .collect()
    } else {
        let mut ms = my_mods.clone();
        ms.sort();
        ms
    };
    if !m.is_empty() && m != "All" && !mods.contains(&m) {
        mods.push(m.clone());
    }
    let mut mod_opts = String::from("<option value=\"All\">All</option>");
    for x in &mods {
        let sel = if *x == m { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(x), esc(x)));
    }

    const COLS: &[(&str, &str)] = &[("module", "Module"), ("name", "Name"), ("root", "Root"), ("expire", "Expire"), ("valid", "Valid")];
    let qs_keep = web::qs(&ctx.params, &["sort", "order", "offset", "max"]);
    let mut thead = String::new();
    for (col, lbl) in COLS {
        let sorted = sort_p == *col;
        let next_ord = if sorted && order_dir == "asc" { "desc" } else { "asc" };
        let cls = if sorted { format!("sortable sorted {}", order_dir) } else { "sortable".into() };
        let extra_q = if qs_keep.is_empty() { String::new() } else { format!("&amp;{}", esc(&qs_keep)) };
        thead.push_str(&format!(
            "<th class=\"{cls}\"><a href=\"/portalTree/index?sort={col}&amp;max={max}&amp;order={next_ord}{extra_q}\">{lbl}</a></th>"
        ));
    }

    let mut tbody = String::new();
    for (rn, r) in rows.iter().enumerate() {
        let tid = i(r, "id").unwrap_or(0);
        let row_cls = if rn % 2 == 0 { "even" } else { "odd" };
        let root_cell = match i(r, "root_id") {
            Some(rid) => {
                let rname = { let x = s(r, "root_name"); if x.is_empty() { format!("#{rid}") } else { x } };
                format!("<a href=\"/portalTreeNode/show/{rid}\">{}</a>", esc(&rname))
            }
            None => match fallback_roots.get(&tid) {
                Some((rid, rname)) => format!("<a href=\"/portalTreeNode/show/{rid}\">{}</a>", esc(rname)),
                None => format!("<a href=\"/portalTree/create_root/{tid}\">Create root node</a>"),
            },
        };
        let mod_txt = { let x = s(r, "module"); if x.is_empty() { format!("#{tid}") } else { x } };
        tbody.push_str(&format!(
            "<tr class=\"{row_cls}\">\
             <td><a href=\"/portalTree/show/{tid}\">{}</a>&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{root_cell}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             <td>{}&nbsp;</td>\
             </tr>",
            esc(&mod_txt),
            esc(&s(r, "name")),
            esc(&fmt_tree_dt(&s(r, "expire"))),
            esc(&fmt_tree_dt(&s(r, "valid")))
        ));
    }

    let pag = if total > max {
        format!("<div class=\"pagination\">{}</div>", crate::tags::paginate("/portalTree/index", &ctx.params, total, max, offset))
    } else {
        String::new()
    };
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let create_href = if !m.is_empty() && m != "All" {
        format!("/portalTree/create?module={}", enc(&m))
    } else {
        "/portalTree/create".into()
    };

    let body = format!(
        r##"<style>
@media only screen and (max-width: 760px), (min-device-width: 768px) and (max-device-width: 1024px) {{
  td:nth-of-type(1):before {{ content: "Module"; }}
  td:nth-of-type(2):before {{ content: "Name"; }}
  td:nth-of-type(3):before {{ content: "Root"; }}
  td:nth-of-type(4):before {{ content: "Expire"; }}
  td:nth-of-type(5):before {{ content: "Valid"; }}
}}
</style>
<div id="content" role="main"><div class="container">
<section class="row"><a href="#list-portalTree" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="create" href="{create_href}">New PortalTree</a></li>
  <li><a class="create" href="/portalTreeNode/fixnodes">Fix Nodes</a></li>
  <li><a class="create" href="/portalTree/importtreeform">Import Tree</a></li>
</ul></div></section>
<section class="row"><div id="list-portalTree" class="col-12 content scaffold-list" role="main">
  <h1>PortalTree List</h1>
  {flash_html}
  <form action="/portalTree/index" method="get" name="searchform" id="searchform">
    <fieldset class="form">
      <div class="fieldcontain" id="tracksearch">
        <label>Search:</label><input type="text" name="q" id="q" value="{q_val}"/><input id="dosearch" name="dosearch" type="submit" value="Search"/>
      </div>
      <div class="fieldcontain" id="listsearch">
        <label>Module:</label>
        <select name="module" id="module" class="module-select " onchange="this.form.submit()">{mod_opts}</select>
      </div>
    </fieldset>
  </form>
  <table class="responsive">
    <thead><tr>{thead}</tr></thead>
    <tbody>{tbody}</tbody>
  </table>
  {pag}
</div></section></div></div>"##,
        q_val = esc(&q)
    );
    ctx.html("PortalTree List", body).await
}

async fn tree_show(mut ctx: Ctx, id: i64) -> Response {
    let db = &ctx.st.db.clone();
    let Some(t) = tree_of(&ctx, id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await;
    };
    if !ctx.can_admin(&s(&t, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to view this").await;
    }
    let root_html = match root_id(&ctx, &t).await {
        Some(rid) => {
            let rname = db.get("portal_tree_node", rid).await.ok().flatten().map(|n| s(&n, "name")).filter(|x| !x.is_empty()).unwrap_or_else(|| format!("#{rid}"));
            format!("<a href=\"/portalTreeNode/show/{rid}\">{}</a>", esc(&rname))
        }
        None => format!("<a href=\"/portalTree/create_root/{id}\">Create root node</a>"),
    };
    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#show-portalTree" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTree/index">PortalTree List</a></li>
  <li><a class="create" href="/portalTree/create">New PortalTree</a></li>
</ul></div></section>
<section class="row"><div id="show-portalTree" class="col-12 content scaffold-show" role="main">
  <h1>Show PortalTree</h1>
  {flash_html}
  <ol class="property-list portalTree">
    <li class="fieldcontain"><span id="module-label" class="property-label">Module</span><div class="property-value" aria-labelledby="module-label">{module}</div></li>
    <li class="fieldcontain"><span id="name-label" class="property-label">Name</span><div class="property-value" aria-labelledby="name-label">{name}</div></li>
    <li class="fieldcontain"><span id="root-label" class="property-label">Root</span><div class="property-value" aria-labelledby="root-label">{root_html}</div></li>
    <li class="fieldcontain"><span id="expire-label" class="property-label">Expire</span><div class="property-value" aria-labelledby="expire-label">{expire}</div></li>
    <li class="fieldcontain"><span id="valid-label" class="property-label">Valid</span><div class="property-value" aria-labelledby="valid-label">{valid}</div></li>
  </ol>
  <form action="/portalTree/delete/{id}" method="post">
    {csrf}
    <fieldset class="buttons">
      <a class="edit" href="/portalTree/edit/{id}">Edit</a>
      <a class="save" href="/portalTree/exporttree/{id}">Export</a>
      <a class="save" href="/portalTree/exporttree/{id}?staff=on">Export with Role Holders</a>
      <a class="save" href="/portalTree/importtreeform">Import</a>
      <input class="delete" type="submit" value="Delete" onclick="return confirm('Are you sure?');" />
    </fieldset>
  </form>
</div></section></div></div>"##,
        module = esc(&s(&t, "module")),
        name = esc(&s(&t, "name")),
        expire = esc(&fmt_tree_dt(&s(&t, "expire"))),
        valid = esc(&fmt_tree_dt(&s(&t, "valid")))
    );
    ctx.html("Show PortalTree", body).await
}

async fn tree_create_or_edit(mut ctx: Ctx, id: Option<i64>) -> Response {
    let db = &ctx.st.db.clone();
    let is_admin = ctx.is_admin();
    let my_mods = if is_admin { vec![] } else { web::admin_list(db, ctx.user.as_ref().unwrap()).await };
    if !is_admin && my_mods.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let is_new = id.is_none();
    let t = match id {
        Some(tid) => match tree_of(&ctx, tid).await {
            Some(row) => {
                if !is_admin && !my_mods.contains(&s(&row, "module")) {
                    return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
                }
                row
            }
            None => return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await,
        },
        None => {
            let mut row = Row::new();
            let m = ctx.p("module");
            if !m.is_empty() {
                row.insert("module".into(), json!(m));
            }
            row
        }
    };

    let mut mods: Vec<String> = if is_admin {
        db.rows("select name from portal_module order by name", &db::no_params()).await.unwrap_or_default().iter().map(|r| s(r, "name")).collect()
    } else {
        let mut ms = my_mods.clone();
        ms.sort();
        ms
    };
    let cur_mod = s(&t, "module");
    if !cur_mod.is_empty() && !mods.contains(&cur_mod) {
        mods.push(cur_mod.clone());
    }
    if mods.is_empty() {
        mods.push("portal".into());
    }
    let mut mod_opts = String::new();
    if cur_mod.is_empty() {
        mod_opts.push_str("<option value=\"\">- choose a module -</option>");
    }
    for m in &mods {
        let sel = if *m == cur_mod { " selected=\"selected\"" } else { "" };
        mod_opts.push_str(&format!("<option value=\"{}\"{sel}>{}</option>", esc(m), esc(m)));
    }

    let extra_dates = if is_new {
        String::new()
    } else {
        let dt_val = |col: &str| s(&t, col).replace(' ', "T").chars().take(16).collect::<String>();
        format!(
            "<div class=\"fieldcontain\"><label for=\"expire\">Expire</label><input type=\"datetime-local\" name=\"expire\" id=\"expire\" value=\"{}\"/></div>\
             <div class=\"fieldcontain\"><label for=\"valid\">Valid</label><input type=\"datetime-local\" name=\"valid\" id=\"valid\" value=\"{}\"/></div>",
            esc(&dt_val("expire")),
            esc(&dt_val("valid"))
        )
    };

    let csrf = ctx.csrf_field();
    let flash = s(&ctx.sess, "flash");
    let flash_html = if flash.is_empty() { String::new() } else { format!("<div class=\"message\" role=\"status\">{}</div>", esc(&flash)) };
    let action = match id {
        Some(tid) => format!("/portalTree/update/{tid}"),
        None => "/portalTree/save".into(),
    };
    let kind = if is_new { "create" } else { "edit" };
    let heading = if is_new { "Create PortalTree" } else { "Edit PortalTree" };
    let btn = if is_new { "Create" } else { "Update" };
    let new_li = if !is_new { "<li><a class=\"create\" href=\"/portalTree/create\">New PortalTree</a></li>" } else { "" };

    let body = format!(
        r##"<div id="content" role="main"><div class="container">
<section class="row"><a href="#{kind}-portalTree" class="skip" tabindex="-1">Skip to content&hellip;</a>
<div class="nav" role="navigation"><ul>
  <li><a class="home" href="/">Home</a></li>
  <li><a class="list" href="/portalTree/index">PortalTree List</a></li>
  {new_li}
</ul></div></section>
<section class="row"><div id="{kind}-portalTree" class="col-12 content scaffold-{kind}" role="main">
  <h1>{heading}</h1>
  {flash_html}
  <form action="{action}" method="post">
    {csrf}
    <fieldset class="form">
      <div class="fieldcontain"><label for="module">Module</label><select name="module" id="module" class="module-select ">{mod_opts}</select></div>
      <div class="fieldcontain required"><label for="name">Name<span class="required-indicator">*</span></label><input type="text" name="name" id="name" value="{name}" required="required"/></div>
      {extra_dates}
    </fieldset>
    <fieldset class="buttons"><input class="save" type="submit" value="{btn}" /></fieldset>
  </form>
  {form_js}
</div></section></div></div>"##,
        name = esc(&s(&t, "name")),
        form_js = crate::tags::form_js()
    );
    ctx.html(heading, body).await
}

async fn tree_save(mut ctx: Ctx) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect("/portalTree/create").await;
    }
    let module = ctx.p("module").trim().to_string();
    let name = ctx.p("name").trim().to_string();
    if !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect("/portalTree/create").await;
    }
    match ctx.db().insert("portal_tree", &row! {"module"=>module,"name"=>name}).await {
        Ok(tid) => {
            crate::cache::MENUS.clear();
            ctx.flash(&format!("PortalTree {tid} created"));
            ctx.redirect(&format!("/portalTree/show/{tid}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not create PortalTree: {e}"));
            ctx.redirect("/portalTree/create").await
        }
    }
}

async fn tree_update(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.redirect(&format!("/portalTree/edit/{id}")).await;
    }
    let Some(existing) = tree_of(&ctx, id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await;
    };
    let module = ctx.p("module").trim().to_string();
    let name = ctx.p("name").trim().to_string();
    if !ctx.can_admin(&s(&existing, "module")).await || !ctx.can_admin(&module).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    if name.is_empty() {
        ctx.flash("Name is required");
        return ctx.redirect(&format!("/portalTree/edit/{id}")).await;
    }
    let opt_dt = |k: &str| {
        let v = ctx.p(k).trim().to_string();
        if v.is_empty() { Value::Null } else { json!(v.replace('T', " ")) }
    };
    match ctx
        .db()
        .update("portal_tree", id, &row! {"module"=>module,"name"=>name,"expire"=>opt_dt("expire"),"valid"=>opt_dt("valid")})
        .await
    {
        Ok(_) => {
            crate::cache::MENUS.clear();
            ctx.flash(&format!("PortalTree {id} updated"));
            ctx.redirect(&format!("/portalTree/show/{id}")).await
        }
        Err(e) => {
            ctx.flash(&format!("Could not update PortalTree: {e}"));
            ctx.redirect(&format!("/portalTree/edit/{id}")).await
        }
    }
}

async fn tree_delete(mut ctx: Ctx, id: i64) -> Response {
    if ctx.method != "POST" {
        return ctx.status(StatusCode::METHOD_NOT_ALLOWED, "POST required").await;
    }
    let Some(existing) = tree_of(&ctx, id).await else {
        return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await;
    };
    if !ctx.can_admin(&s(&existing, "module")).await {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let db = ctx.db();
    let _ = db.exec("update portal_tree set root_id=null where id=:id", &row! {"id"=>id}).await;
    let _ = db.exec("delete from portal_tree_node_user where node_id in (select id from portal_tree_node where tree_id=:id)", &row! {"id"=>id}).await;
    let _ = db.exec("delete from portal_tree_node where tree_id=:id", &row! {"id"=>id}).await;
    let _ = db.delete("portal_tree", id).await;
    crate::cache::MENUS.clear();
    ctx.flash(&format!("PortalTree {id} deleted"));
    ctx.redirect("/portalTree/index").await
}

/// /user/api_list?q= : users for the node-user picker (select2), for anyone who manages a module.
pub async fn user_api_list(ctx: Ctx) -> Response {
    let Some(u) = ctx.user.clone() else { return ctx.need_login().await };
    if !ctx.is_admin() && crate::web::admin_list(ctx.db(), &u).await.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let q = ctx.p("q");
    let like = format!("%{}%", q.trim());
    let rows = ctx.db().rows("select id, name, userid from portal_user where is_active is not false and (name ilike :q or userid ilike :q) order by name limit 30", &row! {"q"=>like}).await.unwrap_or_default();
    ctx.json(json!({"users": rows.iter().map(|r| json!({"id": i(r, "id"), "name": format!("{} ({})", s(r, "name"), s(r, "userid"))})).collect::<Vec<_>>()})).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_lines_html_detects_identical_new_and_changed() {
        let (same_html, same_changed) = diff_lines_html("a\nb\n", "a\nb\n");
        assert!(!same_changed);
        assert!(same_html.is_empty());

        let (new_html, new_changed) = diff_lines_html("", "line1\nline2");
        assert!(new_changed);
        assert!(new_html.contains("/dev/null"));
        assert!(new_html.contains("+line1"));

        let (chg_html, chg_changed) = diff_lines_html("keep\nold\nend", "keep\nnew\nend");
        assert!(chg_changed);
        assert!(chg_html.contains("-old"));
        assert!(chg_html.contains("+new"));
    }

    #[test]
    fn fmt_tree_dt_normalizes_iso_and_preserves_plain() {
        assert_eq!(fmt_tree_dt(""), "");
        assert_eq!(fmt_tree_dt("2026-11-22T07:50:00.123Z"), "2026-11-22 07:50:00");
        assert_eq!(fmt_tree_dt("2026-11-22 07:50:00"), "2026-11-22 07:50:00");
    }
}
