//! Trees: nested-set maintenance and the menus built from them.

use crate::db::{b, i, s, Db, Row};
use crate::row;
use crate::web::esc;
use serde_json::Value;

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

fn node_data(n: &Row, key: &str) -> Option<String> {
    let d = s(n, "data");
    serde_json::from_str::<Value>(&d).ok().and_then(|v| v.get(key).map(crate::db::vs)).filter(|x| !x.is_empty())
}

pub fn node_icon(n: &Row) -> String {
    if let Some(ic) = node_data(n, "icon") {
        return format!("<i class='bi bi-{}'></i> ", esc(&ic));
    }
    if let Some(img) = node_data(n, "img") {
        return format!("<img src='/download/portal/{}' class='img-fluid' style='max-height:1.5em;max-width:1.5em'/> ", esc(&img));
    }
    String::new()
}

async fn tree_nodes(db: &Db, module: &str, name: &str) -> Option<(Row, Vec<Row>)> {
    crate::cache::MENUS.get_or(format!("{}\u{1}{}", module, name), tree_nodes_db(db, module, name)).await
}

async fn tree_nodes_db(db: &Db, module: &str, name: &str) -> Option<(Row, Vec<Row>)> {
    let tree = db.first("select * from tree where module=:m and name=:n order by id limit 1", &row! {"m"=>module,"n"=>name}).await.ok().flatten()?;
    let nodes = db.rows("select * from tree_node where tree_id=:t and disabled is not true order by lft nulls last, id", &row! {"t"=>i(&tree, "id")}).await.ok()?;
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

/// Sidebar menu for a page or tracker whose `side_menu` names a tree `<name>_menu`.
pub async fn side_menu(db: &Db, name: &str) -> String {
    let Some((tree, nodes)) = tree_nodes(db, "portal", name).await else { return String::new() };
    let mut o = String::new();
    for group in kids(&nodes, root_id(&tree, &nodes)) {
        let items = kids(&nodes, i(group, "id"));
        if items.is_empty() {
            o.push_str(&format!("<li class='nav-item'><a class='nav-link collapsed' href='{}'>{}<span>{}</span></a></li>", esc(&node_url(group)), node_icon(group), esc(&s(group, "name"))));
        } else {
            let ulid: String = s(group, "name").to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
            o.push_str(&format!(
                "<li class='nav-item'><a class='nav-link collapsed' data-bs-target='#{id}-nav' data-bs-toggle='collapse' href='#'>{ic}<span>{n}</span><i class='bi bi-chevron-down ms-auto'></i></a><ul id='{id}-nav' class='nav-content collapse' data-bs-parent='#sidebar-nav'>",
                id = ulid, ic = node_icon(group), n = esc(&s(group, "name"))
            ));
            for it in items {
                o.push_str(&format!("<li><a href='{}'>{}<span>{}</span></a></li>", esc(&node_url(it)), node_icon(it), esc(&s(it, "name"))));
            }
            o.push_str("</ul></li>");
        }
    }
    o
}

/// Recompute lft/rgt for a whole tree from the parent links.
pub async fn rebuild(db: &Db, tree_id: i64) -> anyhow::Result<()> {
    let nodes = db.rows("select id,parent_id,lft from tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tree_id}).await?;
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
        db.exec("update tree_node set lft=:l, rgt=:r where id=:id", &row! {"l"=>l,"r"=>r,"id"=>id}).await?;
    }
    if let Some(root) = nodes.iter().find(|n| i(n, "parent_id").is_none()).and_then(|n| i(n, "id")) {
        db.exec("update tree set root_id=:r where id=:t and root_id is null", &row! {"r"=>root,"t"=>tree_id}).await?;
    }
    Ok(())
}

/// Nested HTML view of a tree with its users (for /portalTree/show).
pub async fn tree_html(db: &Db, tree_id: i64) -> String {
    let nodes = db.rows("select * from tree_node where tree_id=:t order by lft nulls last, id", &row! {"t"=>tree_id}).await.unwrap_or_default();
    let users = db
        .rows("select tnu.id, tnu.node_id, tnu.role, u.emp_name, u.staffid, u.id as uid from tree_node_user tnu join tree_node n on n.id=tnu.node_id left join iap_user u on u.id=tnu.user_id where n.tree_id=:t order by u.emp_name", &row! {"t"=>tree_id})
        .await
        .unwrap_or_default();
    fn render(nodes: &[Row], users: &[Row], parent: Option<i64>, tree_id: i64, o: &mut String) {
        let ks = kids(nodes, parent);
        if ks.is_empty() {
            return;
        }
        o.push_str("<ul class='tree-list'>");
        for n in ks {
            let id = i(n, "id").unwrap_or(0);
            o.push_str(&format!(
                "<li><a href='/portalTreeNode/show/{id}'><b>{}</b></a>{} <small class='text-muted'>{}</small> <a class='btn btn-sm btn-outline-secondary py-0' href='/portalTreeNode/create?tree_id={tree_id}&parent_id={id}'>+ node</a> <a class='btn btn-sm btn-outline-secondary py-0' href='/portalTreeNodeUser/create?node_id={id}'>+ user</a>",
                esc(&s(n, "name")), if b(n, "disabled") { " <span class='badge bg-secondary'>disabled</span>" } else { "" }, esc(&s(n, "slug"))
            ));
            let us: Vec<&Row> = users.iter().filter(|u| i(u, "node_id") == Some(id)).collect();
            if !us.is_empty() {
                o.push_str("<div class='ms-3'>");
                for u in us {
                    o.push_str(&format!("<span class='badge bg-light text-dark border me-1'><a href='/portalTreeNodeUser/show/{}'>{} ({})</a> &middot; {}</span>", i(u, "id").unwrap_or(0), esc(&s(u, "emp_name")), esc(&s(u, "staffid")), esc(&s(u, "role"))));
                }
                o.push_str("</div>");
            }
            render(nodes, users, Some(id), tree_id, o);
            o.push_str("</li>");
        }
        o.push_str("</ul>");
    }
    let mut o = String::new();
    render(&nodes, &users, None, tree_id, &mut o);
    if o.is_empty() {
        o = format!("<p>No nodes yet. <a href='/portalTreeNode/create?tree_id={}'>Create the root node</a></p>", tree_id);
    }
    o
}
