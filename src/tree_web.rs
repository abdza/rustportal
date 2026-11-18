//! The tree admin screens, laid out like the Grails portal's: the tree page links
//! to its root node, and the node page is a jsTree of the whole tree with the
//! selected node's details, its role holders (add / change / remove in a dialog),
//! drag-and-drop moving, and New Node / Edit / Delete / Edit Domain buttons.

use crate::db::{self, b, i, s, Row};
use crate::row;
use crate::tree::{export_tree, import_tree, rebuild, ImportStats};
use crate::web::{esc, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::{json, Value};

async fn tree_of(ctx: &Ctx, tree_id: i64) -> Option<Row> {
    ctx.db().get("tree", tree_id).await.ok().flatten()
}

/// A node and its tree, if the user may administer the tree's module.
async fn node_and_tree(ctx: &Ctx, id: i64) -> Result<(Row, Row), (StatusCode, &'static str)> {
    let node = ctx.db().get("tree_node", id).await.ok().flatten().ok_or((StatusCode::NOT_FOUND, "Tree node not found"))?;
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
    ctx.db().first("select id from tree_node where tree_id=:t and parent_id is null order by lft nulls last, id limit 1", &row! {"t"=>i(tree, "id")}).await.ok().flatten().and_then(|r| i(&r, "id"))
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
    let who = if us.len() == 1 { format!(" - {}: {} ({})", esc(&s(us[0], "role")), esc(&s(us[0], "emp_name")), esc(&s(us[0], "staffid"))) } else { String::new() };
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
    let nodes = ctx.db().rows("select id, parent_id, name from tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tid}).await.unwrap_or_default();
    let users = ctx.db().rows("select tnu.node_id, tnu.role, u.emp_name, u.staffid from tree_node_user tnu join tree_node n on n.id=tnu.node_id join iap_user u on u.id=tnu.user_id where n.tree_id=:t", &row! {"t"=>tid}).await.unwrap_or_default();
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
    let users = db.rows("select tnu.id, tnu.user_id, tnu.role, u.emp_name, u.staffid from tree_node_user tnu join iap_user u on u.id=tnu.user_id where tnu.node_id=:n order by tnu.role, u.emp_name", &row! {"n"=>id}).await.unwrap_or_default();
    // ancestors, root first (the Grails portal's getfullpath)
    let path = db.rows("select name from tree_node where tree_id=:t and lft<:l and rgt>:r order by lft", &row! {"t"=>i(&node, "tree_id"),"l"=>i(&node, "lft").unwrap_or(0),"r"=>i(&node, "rgt").unwrap_or(0)}).await.unwrap_or_default();
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
            "users": users.iter().map(|u| json!({"id": i(u, "id"), "user_id": i(u, "user_id"), "name": format!("{} ({})", s(u, "emp_name"), s(u, "staffid")), "role": s(u, "role"), "staff_id": s(u, "staffid")})).collect::<Vec<_>>(),
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
    let all = db.rows("select id, parent_id from tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tid}).await.unwrap_or_default();
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
    let _ = db.exec("update tree_node set parent_id=:r where id=:o", &row! {"r"=>r,"o"=>o}).await;
    // rebuild() keeps siblings in lft order, so number them in their new order first
    for (ix, sid) in sibs.iter().enumerate() {
        let _ = db.exec("update tree_node set lft=:l where id=:id", &row! {"l"=>ix as i64,"id"=>*sid}).await;
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
        Some(x) => db.get("tree_node_user", x).await.ok().flatten().filter(|r| i(r, "node_id") == Some(nid)),
        None => None,
    };
    match ctx.p("nodeaction").as_str() {
        "new" => match user {
            Some(u) if !role.is_empty() => {
                let _ = db.insert("tree_node_user", &row! {"node_id"=>nid,"user_id"=>u,"role"=>role}).await;
                ctx.flash("User added to the node");
            }
            _ => ctx.flash("Choose a user and enter a role"),
        },
        "edit" => match (tnu, user) {
            (Some(t), Some(u)) if !role.is_empty() => {
                let tid = i(&t, "id").unwrap_or(0);
                let _ = db.update("tree_node_user", tid, &row! {"user_id"=>u,"role"=>role}).await;
                // the row now belongs to someone else: the previous holder stops acting as it
                if i(&t, "user_id") != Some(u) {
                    let _ = db.exec("update iap_user set roletargetid=null where roletargetid=:t and id<>:u", &row! {"t"=>tid,"u"=>u}).await;
                }
                ctx.flash("Node user updated");
            }
            _ => ctx.flash("Choose a user and enter a role"),
        },
        "delete" => {
            if let Some(t) = tnu {
                let tid = i(&t, "id").unwrap_or(0);
                let _ = db.exec("update iap_user set roletargetid=null where roletargetid=:t", &row! {"t"=>tid}).await;
                let _ = db.delete("tree_node_user", tid).await;
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
            match ctx.db().insert("tree_node", &d).await {
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
            match ctx.db().update("tree_node", id, &d).await {
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
    let all = db.rows("select id, parent_id from tree_node where tree_id=:t", &row! {"t"=>tid}).await.unwrap_or_default();
    let mut gone = vec![id];
    let mut ix = 0;
    while ix < gone.len() {
        let p = gone[ix];
        gone.extend(all.iter().filter(|n| i(n, "parent_id") == Some(p)).filter_map(|n| i(n, "id")));
        ix += 1;
    }
    let list = db::lit(&json!(gone));
    let _ = db.exec(&format!("update iap_user set roletargetid=null where roletargetid in (select id from tree_node_user where node_id in {})", list), &db::no_params()).await;
    let _ = db.exec(&format!("delete from tree_node_user where node_id in {}", list), &db::no_params()).await;
    let _ = db.exec(&format!("update tree set root_id=null where root_id in {}", list), &db::no_params()).await;
    let _ = db.exec(&format!("delete from tree_node where id in {}", list), &db::no_params()).await;
    let _ = rebuild(&db, tid).await;
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
    let trees = ctx.db().rows("select id, module from tree order by id", &db::no_params()).await.unwrap_or_default();
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

/// /portalTree/create_root, exporttree, importtreeform, importtree
pub async fn tree_action(mut ctx: Ctx, action: &str, id: Option<i64>) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    match action {
        "create_root" | "exporttree" => {
            let Some(tree) = tree_of(&ctx, id.unwrap_or(0)).await else { return ctx.status(StatusCode::NOT_FOUND, "Tree not found").await };
            if !ctx.can_admin(&s(&tree, "module")).await {
                return ctx.status(StatusCode::FORBIDDEN, "You are not authorized to manage this tree").await;
            }
            let tid = i(&tree, "id").unwrap_or(0);
            if action == "create_root" {
                if root_id(&ctx, &tree).await.is_none() {
                    if let Ok(r) = ctx.db().insert("tree_node", &row! {"tree_id"=>tid,"name"=>s(&tree, "name"),"disabled"=>false}).await {
                        let _ = ctx.db().update("tree", tid, &row! {"root_id"=>r}).await;
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
                            <input type="submit" name="upload" class="save" value="Import" />
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
                Ok(Value::Object(o)) => vec![o], // a single tree object is accepted too
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
            let mut stats = ImportStats::default();
            let mut last = None;
            let mut errs = vec![];
            for t in &trees {
                match import_tree(ctx.db(), t, None, staff, &mut stats).await {
                    Ok(id) => last = Some(id),
                    Err(e) => errs.push(format!("{}: {}", s(t, "name"), e)),
                }
            }
            crate::cache::MENUS.clear();
            let mut msg = format!("Tree import: {}", stats.summary());
            if !errs.is_empty() {
                msg.push_str(&format!(". Failed: {}", errs.join("; ")));
            }
            if !stats.users_skipped.is_empty() {
                msg.push_str(&format!(". Skipped: {}", stats.users_skipped.iter().take(10).cloned().collect::<Vec<_>>().join("; ")));
            }
            ctx.flash(&msg);
            match (trees.len(), last) {
                (1, Some(id)) => ctx.redirect(&format!("/portalTree/show/{}", id)).await,
                _ => ctx.redirect("/portalTree/index").await,
            }
        }
        _ => ctx.status(StatusCode::NOT_FOUND, "Page not found").await,
    }
}

/// /user/api_list?q= : users for the node-user picker (select2), for anyone who manages a module.
pub async fn user_api_list(ctx: Ctx) -> Response {
    let Some(u) = ctx.user.clone() else { return ctx.need_login().await };
    if !ctx.is_admin() && crate::web::admin_list(ctx.db(), &u).await.is_empty() {
        return ctx.status(StatusCode::FORBIDDEN, "Not allowed").await;
    }
    let q = ctx.p("q");
    let like = format!("%{}%", q.trim());
    let rows = ctx.db().rows("select id, emp_name, staffid from iap_user where isactive is not false and (emp_name ilike :q or staffid ilike :q or new_staffid ilike :q) order by emp_name limit 30", &row! {"q"=>like}).await.unwrap_or_default();
    ctx.json(json!({"users": rows.iter().map(|r| json!({"id": i(r, "id"), "name": format!("{} ({})", s(r, "emp_name"), s(r, "staffid"))})).collect::<Vec<_>>()})).await
}
