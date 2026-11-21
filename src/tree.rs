//! Trees: nested-set maintenance and the menus built from them.

use crate::db::{i, s, Db, Row};
use crate::row;
use crate::web::esc;
use serde_json::{json, Value};

/// URL for a menu node. The slug is `page:module:slug[:arg1..]`,
/// `tracker:module:slug`, `run:module:slug`, `file:module:slug` or `link:url`.
pub fn node_url(n: &Row) -> String {
    let slug = s(n, "slug");
    let t: Vec<&str> = slug.split(':').collect();
    if t.len() < 2 {
        return String::new();
    }
    let rest = if t.len() > 3 { format!("/{}", t[3..].join("/")) } else { String::new() };
    match t[0] {
        "page" if t.len() > 2 => format!("/view/{}/{}{}", t[1], t[2], rest),
        "tracker" if t.len() > 2 => format!("/{}/{}/list", t[1], t[2]),
        "tracker" => format!("/portal/{}/list", t[1]),
        "run" if t.len() > 2 => format!("/run/{}/{}{}", t[1], t[2], rest),
        "file" if t.len() > 2 => format!("/download/{}/{}", t[1], t[2]),
        "link" => t[1..].join(":"),
        _ => String::new(),
    }
}

/// A value from a node's `data`: `key=value;key2=value2` as the Grails portals
/// write it (key case-insensitive, last one wins), or a JSON object.
pub fn node_data(n: &Row, key: &str) -> Option<String> {
    let d = s(n, "data");
    let d = d.trim();
    if d.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(d) {
            return v.get(key).map(crate::db::vs).filter(|x| !x.is_empty());
        }
    }
    let mut found = None;
    for part in d.split(';') {
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case(key.trim()) && !v.trim().is_empty() {
                found = Some(v.trim().to_string());
            }
        }
    }
    found
}

pub fn node_icon(n: &Row) -> String {
    if let Some(ic) = node_data(n, "icon") {
        return format!("<i class='bi bi-{}'></i> ", esc(&ic));
    }
    if let Some(img) = node_data(n, "img") {
        return format!("<img src='/download/portal/{}' class='img-fluid' style='max-height: 1.5em; max-width: 1.5em;'/>", esc(&img));
    }
    String::new()
}

async fn tree_nodes(db: &Db, module: &str, name: &str) -> Option<(Row, Vec<Row>)> {
    crate::cache::MENUS.get_or(format!("{}\u{1}{}", module, name), tree_nodes_db(db, module, name)).await
}

async fn tree_nodes_db(db: &Db, module: &str, name: &str) -> Option<(Row, Vec<Row>)> {
    let tree = db.first("select * from portal_tree where module=:m and name=:n order by id limit 1", &row! {"m"=>module,"n"=>name}).await.ok().flatten()?;
    let nodes = db.rows("select * from portal_tree_node where tree_id=:t and disabled is not true order by lft nulls last, id", &row! {"t"=>i(&tree, "id")}).await.ok()?;
    Some((tree, nodes))
}

fn kids<'a>(nodes: &'a [Row], parent: Option<i64>) -> Vec<&'a Row> {
    nodes.iter().filter(|n| i(n, "parent_id") == parent).collect()
}

fn root_id(tree: &Row, nodes: &[Row]) -> Option<i64> {
    i(tree, "root_id").or_else(|| nodes.iter().find(|n| i(n, "parent_id").is_none()).and_then(|n| i(n, "id")))
}

/// Header mega menu: root -> columns -> groups -> items.
pub async fn mega_menu(db: &Db, module: &str, name: &str) -> String {
    let Some((tree, nodes)) = tree_nodes(db, module, name).await else { return String::new() };
    let mut o = String::new();
    for col in kids(&nodes, root_id(&tree, &nodes)) {
        o.push_str("<div class='col-lg-3 col-6'><div class='col-megamenu p-2'>");
        for group in kids(&nodes, i(col, "id")) {
            let items = kids(&nodes, i(group, "id"));
            if items.is_empty() {
                o.push_str(&format!("<li><a href='{}'>{}{}</a></li>", esc(&node_url(group)), node_icon(group), esc(&s(group, "name"))));
            } else {
                o.push_str(&format!("<h3 class='title'>{}</h3><ul class='list-unstyled'>", esc(&s(group, "name"))));
                for it in items {
                    o.push_str(&format!("<li><a href='{}'>{}{}</a></li>", esc(&node_url(it)), node_icon(it), esc(&s(it, "name"))));
                }
                o.push_str("</ul><hr/>");
            }
        }
        o.push_str("</div></div>");
    }
    o
}

/// Sidebar menu for a page or tracker whose `side_menu` names a tree `<name>_menu`,
/// in the Grails portals' markup. Nodes with `adminonly=true` in their data only
/// show for the module's admins.
pub async fn side_menu(db: &Db, name: &str, module_admin: bool) -> String {
    let Some((tree, nodes)) = tree_nodes(db, "portal", name).await else { return String::new() };
    let visible = |n: &Row| module_admin || node_data(n, "adminonly").as_deref() != Some("true");
    let mut o = String::new();
    for group in kids(&nodes, root_id(&tree, &nodes)).into_iter().filter(|g| visible(g)) {
        let items = kids(&nodes, i(group, "id"));
        if items.is_empty() {
            o.push_str(&format!("<li class='nav-item'><a class='nav-link' href='{}'>{} {}</a></li>", esc(&node_url(group)), node_icon(group), esc(&s(group, "name"))));
        } else {
            let ulid = s(group, "name").replace(' ', "-").to_lowercase();
            o.push_str(&format!(
                "<li class='nav-item'><a class='nav-link collapsed' data-bs-target='#{id}-nav' data-bs-toggle='collapse' href='#'>{ic} {n}<i class='bi bi-chevron-down ms-auto'></i></a><ul class='nav-content collapse' data-bs-parent='#sidebar-nav' id='{id}-nav'>",
                id = esc(&ulid), ic = node_icon(group), n = esc(&s(group, "name"))
            ));
            for it in items.into_iter().filter(|x| visible(x)) {
                o.push_str(&format!("<li><a href='{}'>{} {}</a></li>", esc(&node_url(it)), node_icon(it), esc(&s(it, "name"))));
            }
            o.push_str("</ul></li>");
        }
    }
    o
}

/// Recompute lft/rgt for a whole tree from the parent links.
pub async fn rebuild(db: &Db, tree_id: i64) -> anyhow::Result<()> {
    let nodes = db.rows("select id,parent_id,lft from portal_tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tree_id}).await?;
    fn walk(nodes: &[Row], id: i64, counter: &mut i64, out: &mut Vec<(i64, i64, i64)>) {
        let lft = *counter;
        *counter += 1;
        for c in nodes.iter().filter(|n| i(n, "parent_id") == Some(id)) {
            walk(nodes, i(c, "id").unwrap_or(0), counter, out);
        }
        out.push((id, lft, *counter));
        *counter += 1;
    }
    let mut counter = 1;
    let mut out = vec![];
    for r in nodes.iter().filter(|n| i(n, "parent_id").is_none()) {
        walk(&nodes, i(r, "id").unwrap_or(0), &mut counter, &mut out);
    }
    for (id, l, r) in out {
        db.exec("update portal_tree_node set lft=:l, rgt=:r where id=:id", &row! {"l"=>l,"r"=>r,"id"=>id}).await?;
    }
    if let Some(root) = nodes.iter().find(|n| i(n, "parent_id").is_none()).and_then(|n| i(n, "id")) {
        db.exec("update portal_tree set root_id=:r where id=:t and root_id is null", &row! {"r"=>root,"t"=>tree_id}).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tree export/import in the Grails portal's format: a list of trees, each with
// its root nodes, every node carrying its children in `nodes` (sibling order
// kept) and, when asked for, its role holders as `users: [{staffid, role}]`.
// Used for single-tree files and for a module's treelist.json alike.

/// A node's key among its siblings: the slug, else the name (as the Grails portal).
fn node_key(n: &Row) -> String {
    let k = s(n, "slug").trim().to_string();
    if k.is_empty() { s(n, "name").trim().to_string() } else { k }
}

/// One tree as the Grails portal exports it.
pub async fn export_tree(db: &Db, tree_id: i64, staff_on: bool) -> anyhow::Result<Value> {
    let tree = db.get("portal_tree", tree_id).await?.ok_or_else(|| anyhow::anyhow!("tree {} not found", tree_id))?;
    let nodes = db.rows("select * from portal_tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tree_id}).await?;
    let users = if staff_on {
        db.rows("select tnu.node_id, tnu.role, u.userid as staffid from portal_tree_node_user tnu join portal_tree_node n on n.id=tnu.node_id join portal_user u on u.id=tnu.user_id where n.tree_id=:t order by 3, 2", &row! {"t"=>tree_id}).await?
    } else {
        vec![]
    };
    fn node(nodes: &[Row], users: &[Row], n: &Row, staff_on: bool) -> Value {
        let id = i(n, "id");
        let us: Value = if staff_on {
            Value::Array(users.iter().filter(|u| i(u, "node_id") == id).map(|u| serde_json::json!({"staffid": s(u, "staffid"), "role": s(u, "role")})).collect())
        } else {
            Value::Null
        };
        let opt = |k: &str| n.get(k).cloned().unwrap_or(Value::Null);
        serde_json::json!({
            "name": opt("name"), "slug": opt("slug"), "data": opt("data"), "domain": opt("domain"), "domainid": opt("domainid"),
            "domainkeyfield": Value::Null, "domainkey": Value::Null, "mainrole": opt("mainrole"), "hiderole": opt("hiderole"),
            "disabled": opt("disabled"), "users": us,
            "nodes": kids(nodes, id).into_iter().map(|c| node(nodes, users, c, staff_on)).collect::<Vec<_>>(),
        })
    }
    let root = i(&tree, "root_id").and_then(|r| nodes.iter().find(|n| i(n, "id") == Some(r))).map(node_key);
    let date = |k: &str| { let v = s(&tree, k); if v.is_empty() { Value::Null } else { Value::String(v.chars().take(10).collect()) } };
    Ok(serde_json::json!({
        "module": s(&tree, "module"), "name": s(&tree, "name"), "valid": date("valid"), "expire": date("expire"), "root": root,
        "nodes": kids(&nodes, None).into_iter().map(|n| node(&nodes, &users, n, staff_on)).collect::<Vec<_>>(),
    }))
}

#[derive(Default, Debug)]
pub struct ImportStats {
    pub trees_added: usize,
    pub trees_updated: usize,
    pub nodes_added: usize,
    pub nodes_updated: usize,
    pub users_added: usize,
    pub users_skipped: Vec<String>,
}

impl ImportStats {
    pub fn summary(&self) -> String {
        let mut o = vec![];
        if self.trees_added > 0 { o.push(format!("{} tree(s) added", self.trees_added)); }
        if self.trees_updated > 0 { o.push(format!("{} tree(s) updated", self.trees_updated)); }
        o.push(format!("{} node(s) added, {} updated", self.nodes_added, self.nodes_updated));
        if self.users_added > 0 { o.push(format!("{} role holder(s) added", self.users_added)); }
        if !self.users_skipped.is_empty() { o.push(format!("{} role holder(s) skipped (staff id not found here)", self.users_skipped.len())); }
        o.join(", ")
    }
}

/// Turn this portal's older flat export (`nodes` with `id`/`parent_id`) into the nested form.
fn nest_flat(nodes: &[Row]) -> Vec<Value> {
    fn build(nodes: &[Row], parent: Option<i64>) -> Vec<Value> {
        nodes.iter().filter(|n| i(n, "parent_id") == parent).map(|n| {
            let mut o = n.clone();
            o.remove("id");
            o.remove("parent_id");
            o.insert("nodes".into(), Value::Array(build(nodes, i(n, "id"))));
            Value::Object(o)
        }).collect()
    }
    let ids: Vec<Option<i64>> = nodes.iter().map(|n| i(n, "id")).collect();
    // nodes whose parent is not in the list are roots
    let mut out = build(nodes, None);
    for n in nodes.iter().filter(|n| i(n, "parent_id").is_some() && !ids.contains(&i(n, "parent_id"))) {
        let mut o = n.clone();
        o.remove("id");
        o.remove("parent_id");
        o.insert("nodes".into(), Value::Array(build(nodes, i(n, "id"))));
        out.push(Value::Object(o));
    }
    out
}

/// Create or update one tree from its exported form (the Grails portal's merge
/// rules): the tree is matched by module and name, nodes by slug-or-name among
/// their siblings; nothing here is deleted, and role holders are only added.
/// `module` overrides the module named in the file. Returns the tree id.
pub async fn import_tree(db: &Db, itree: &Row, module: Option<&str>, staff_on: bool, st: &mut ImportStats) -> anyhow::Result<i64> {
    let name = s(itree, "name");
    if name.trim().is_empty() {
        anyhow::bail!("a tree in the file has no name");
    }
    let module = module.map(String::from).unwrap_or_else(|| { let m = s(itree, "module"); if m.is_empty() { "portal".into() } else { m } });
    let date = |k: &str| { let v = s(itree, k); if v.len() >= 10 { Value::String(v[..10].to_string()) } else { Value::Null } };
    let tree_id = match db.first("select id from portal_tree where module=:m and name=:n order by id limit 1", &row! {"m"=>module.clone(),"n"=>name.clone()}).await? {
        Some(t) => {
            let id = i(&t, "id").unwrap_or(0);
            db.update("portal_tree", id, &row! {"valid"=>date("valid"),"expire"=>date("expire")}).await?;
            st.trees_updated += 1;
            id
        }
        None => {
            st.trees_added += 1;
            db.insert("portal_tree", &row! {"module"=>module.clone(),"name"=>name.clone(),"valid"=>date("valid"),"expire"=>date("expire")}).await?
        }
    };
    let inodes: Vec<Value> = match itree.get("nodes") {
        Some(Value::Array(a)) => {
            let rows: Vec<Row> = a.iter().filter_map(|v| v.as_object().cloned()).collect();
            if rows.iter().any(|n| n.contains_key("parent_id")) { nest_flat(&rows) } else { a.clone() }
        }
        _ => vec![],
    };
    import_nodes(db, tree_id, None, &inodes, staff_on, st).await?;
    rebuild(db, tree_id).await?;
    let root_key = s(itree, "root");
    if !root_key.is_empty() {
        let roots = db.rows("select * from portal_tree_node where tree_id=:t and parent_id is null order by lft nulls last, id", &row! {"t"=>tree_id}).await?;
        if let Some(r) = roots.iter().find(|n| node_key(n) == root_key) {
            db.update("portal_tree", tree_id, &row! {"root_id"=>i(r, "id")}).await?;
        }
    }
    Ok(tree_id)
}

fn import_nodes<'a>(db: &'a Db, tree_id: i64, parent: Option<i64>, inodes: &'a [Value], staff_on: bool, st: &'a mut ImportStats) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + 'a>> {
    Box::pin(async move {
        let existing = match parent {
            Some(p) => db.rows("select * from portal_tree_node where parent_id=:p order by lft nulls last, id", &row! {"p"=>p}).await?,
            None => db.rows("select * from portal_tree_node where tree_id=:t and parent_id is null order by lft nulls last, id", &row! {"t"=>tree_id}).await?,
        };
        let mut pool: Vec<(String, i64)> = existing.iter().map(|n| (node_key(n), i(n, "id").unwrap_or(0))).collect();
        for v in inodes {
            let Some(n) = v.as_object() else { continue };
            let key = node_key(n);
            let disabled = n.get("disabled").map(|d| d == &Value::Bool(true)).unwrap_or(false);
            let mut d = row! {"name"=>n.get("name").cloned().unwrap_or(Value::Null),"slug"=>n.get("slug").cloned().unwrap_or(Value::Null),"data"=>n.get("data").cloned().unwrap_or(Value::Null),
                "domain"=>n.get("domain").cloned().unwrap_or(Value::Null),"domainid"=>i(n, "domainid"),"mainrole"=>n.get("mainrole").cloned().unwrap_or(Value::Null),
                "hiderole"=>n.get("hiderole").cloned().unwrap_or(Value::Null),"disabled"=>disabled};
            let nid = match pool.iter().position(|(k, _)| *k == key) {
                Some(ix) => {
                    let id = pool.remove(ix).1;
                    db.update("portal_tree_node", id, &d).await?;
                    st.nodes_updated += 1;
                    id
                }
                None => {
                    d.insert("tree_id".into(), serde_json::json!(tree_id));
                    d.insert("parent_id".into(), parent.map(|p| serde_json::json!(p)).unwrap_or(Value::Null));
                    st.nodes_added += 1;
                    db.insert("portal_tree_node", &d).await?
                }
            };
            if staff_on {
                if let Some(Value::Array(us)) = n.get("users") {
                    for u in us.iter().filter_map(|u| u.as_object()) {
                        let sid = { let x = s(u, "userid"); if x.is_empty() { s(u, "staffid") } else { x } };
                        let role = s(u, "role");
                        match crate::web::find_user_by_staffid(db, &sid).await {
                            Some(user) => {
                                let p = row! {"n"=>nid,"u"=>i(&user, "id"),"r"=>role.clone()};
                                if db.count("select count(*) from portal_tree_node_user where node_id=:n and user_id=:u and role=:r", &p).await? == 0 {
                                    db.insert("portal_tree_node_user", &row! {"node_id"=>nid,"user_id"=>i(&user, "id"),"role"=>role}).await?;
                                    st.users_added += 1;
                                }
                            }
                            None => st.users_skipped.push(format!("{} ({}) on node '{}'", sid, role, s(n, "name"))),
                        }
                    }
                }
            }
            if let Some(Value::Array(kids)) = n.get("nodes") {
                import_nodes(db, tree_id, Some(nid), kids, staff_on, st).await?;
            }
        }
        Ok(())
    })
}

// ---------------------------------------------------------------------------
// Tree editing for page scripts (menu-setup pages build their sidebars this way).

/// Delete a node with everything under it, its role holders, and a tree's root
/// pointer to any of them; then renumber the tree.
pub async fn delete_node_deep(db: &Db, tree_id: i64, id: i64) -> anyhow::Result<()> {
    let all = db.rows("select id, parent_id from portal_tree_node where tree_id=:t", &row! {"t"=>tree_id}).await?;
    let mut gone = vec![id];
    let mut ix = 0;
    while ix < gone.len() {
        let p = gone[ix];
        gone.extend(all.iter().filter(|n| i(n, "parent_id") == Some(p)).filter_map(|n| i(n, "id")));
        ix += 1;
    }
    let list = crate::db::lit(&json!(gone));
    db.exec(&format!("update portal_user set roletargetid=null where roletargetid in (select id from portal_tree_node_user where node_id in {})", list), &crate::db::no_params()).await?;
    db.exec(&format!("delete from portal_tree_node_user where node_id in {}", list), &crate::db::no_params()).await?;
    db.exec(&format!("update portal_tree set root_id=null where root_id in {}", list), &crate::db::no_params()).await?;
    db.exec(&format!("delete from portal_tree_node where id in {}", list), &crate::db::no_params()).await?;
    rebuild(db, tree_id).await?;
    crate::cache::MENUS.clear();
    Ok(())
}

/// The tree (module, name), created with a root node of the same name when
/// missing. Returns the tree row (with root_id) and whether it was created.
pub async fn ensure_tree(db: &Db, module: &str, name: &str) -> anyhow::Result<(Row, bool)> {
    let mut created = false;
    let tid = match db.first("select id from portal_tree where module=:m and name=:n", &row! {"m"=>module,"n"=>name}).await? {
        Some(t) => i(&t, "id").unwrap_or(0),
        None => {
            created = true;
            db.insert("portal_tree", &row! {"module"=>module,"name"=>name}).await?
        }
    };
    let t = db.first("select * from portal_tree where id=:t", &row! {"t"=>tid}).await?.unwrap_or_default();
    let root_ok = match i(&t, "root_id") {
        Some(r) => db.first("select id from portal_tree_node where id=:r", &row! {"r"=>r}).await?.is_some(),
        None => false,
    };
    if !root_ok {
        let rid = db.insert("portal_tree_node", &row! {"tree_id"=>tid,"name"=>name}).await?;
        db.exec("update portal_tree set root_id=:r where id=:t", &row! {"r"=>rid,"t"=>tid}).await?;
        rebuild(db, tid).await?;
    }
    let t = db.first("select * from portal_tree where id=:t", &row! {"t"=>tid}).await?.unwrap_or_default();
    crate::cache::MENUS.clear();
    Ok((t, created))
}

/// Add a node as the last child of `parent`; returns its id.
pub async fn add_node(db: &Db, parent: i64, name: &str, slug: Option<&str>, data: Option<&str>) -> anyhow::Result<i64> {
    let p = db.first("select id, tree_id from portal_tree_node where id=:p", &row! {"p"=>parent}).await?.ok_or_else(|| anyhow::anyhow!("tree node {} not found", parent))?;
    let tid = i(&p, "tree_id").unwrap_or(0);
    let id = db.insert("portal_tree_node", &row! {"tree_id"=>tid,"parent_id"=>parent,"name"=>name,"slug"=>slug,"data"=>data}).await?;
    rebuild(db, tid).await?;
    crate::cache::MENUS.clear();
    Ok(id)
}
