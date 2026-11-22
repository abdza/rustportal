//! Client branding admin screen (/branding/index, /branding/save, /branding/reset).
//! Matches g6ikram's BrandingController + BrandingService + views/branding/index.gsp:
//! writes PortalSetting rows under module = 'branding' and stores uploaded logo/favicon
//! as FileLinks with allowedroles = 'All'.

use crate::db::{self, i, s, Db};
use crate::row;
use crate::web::{self, esc, Ctx};
use axum::http::StatusCode;
use axum::response::Response;
use serde_json::Value;

pub const COLOR_DEFAULTS: &[(&str, &str, &str, &str)] = &[
    ("primary", "#4154f1", "Primary", "Links, buttons, the selected menu item."),
    ("accent", "#717ff5", "Accent", "The colour a link turns on hover."),
    ("heading", "#012970", "Heading text", "Page titles and the name beside the logo."),
    ("page_bg", "#f6f9ff", "Page background", "Behind the content area."),
    ("header_bg", "#ffffff", "Top bar", "The bar across the top."),
    ("sidebar_bg", "#ffffff", "Side menu", "The menu down the left."),
];

const MAX_IMAGE_BYTES: u64 = 2 * 1024 * 1024;

pub fn valid_color(v: &str) -> bool {
    let b = v.as_bytes();
    if !(b.len() == 4 || b.len() == 7) || b[0] != b'#' {
        return false;
    }
    b[1..].iter().all(|c| c.is_ascii_hexdigit())
}

fn expand_hex(hex: &str) -> String {
    let h = hex.trim();
    if h.len() == 4 {
        let ch: Vec<char> = h.chars().collect();
        format!("#{}{}{}{}{}{}", ch[1], ch[1], ch[2], ch[2], ch[3], ch[3])
    } else {
        h.to_string()
    }
}

fn rgb_channels(hex: &str) -> (u8, u8, u8) {
    let h = expand_hex(hex);
    let parse = |a: &str| u8::from_str_radix(a, 16).unwrap_or(0);
    (parse(&h[1..3]), parse(&h[3..5]), parse(&h[5..7]))
}

/// Blend a hex colour 8% over white (matching BrandingService.tint).
pub fn tint(hex: &str) -> String {
    if !valid_color(hex) {
        return "#4154f1".into();
    }
    let (r, g, b) = rgb_channels(hex);
    let wash = |c: u8| (255.0 - (255.0 - c as f64) * 0.08).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", wash(r), wash(g), wash(b))
}

/// Darken a hex colour by `amount` (0.0..1.0) for button hover/active states.
pub fn darken(hex: &str, amount: f64) -> String {
    if !valid_color(hex) {
        return "#3545c8".into();
    }
    let (r, g, b) = rgb_channels(hex);
    let k = (1.0 - amount).clamp(0.0, 1.0);
    let d = |c: u8| ((c as f64) * k).round().clamp(0.0, 255.0) as u8;
    format!("#{:02x}{:02x}{:02x}", d(r), d(g), d(b))
}

/// Choose white or dark text for readable contrast on a solid button of colour `hex`.
pub fn contrast_fg(hex: &str) -> &'static str {
    if !valid_color(hex) {
        return "#ffffff";
    }
    let (r, g, b) = rgb_channels(hex);
    let luma = 0.299 * (r as f64) + 0.587 * (g as f64) + 0.114 * (b as f64);
    if luma > 186.0 { "#212529" } else { "#ffffff" }
}

/// Build the :root CSS declarations for colours that differ from stock NiceAdmin defaults.
pub fn css_variables(brand: &Value) -> String {
    let mut parts = vec![];
    for (key, fallback, _, _) in COLOR_DEFAULTS {
        let val = brand[key].as_str().unwrap_or("");
        if !val.is_empty() && !val.eq_ignore_ascii_case(fallback) && valid_color(val) {
            let low = val.to_ascii_lowercase();
            parts.push(format!("--brand-{}:{}", key.replace('_', "-"), low));
            if *key == "primary" {
                let (r, g, b) = rgb_channels(&low);
                parts.push(format!("--brand-primary-hover:{}", darken(&low, 0.12)));
                parts.push(format!("--brand-primary-active:{}", darken(&low, 0.18)));
                parts.push(format!("--brand-primary-rgb:{},{},{}", r, g, b));
                parts.push(format!("--brand-primary-fg:{}", contrast_fg(&low)));
                parts.push(format!("--bs-primary:{}", low));
                parts.push(format!("--bs-primary-rgb:{},{},{}", r, g, b));
                parts.push(format!("--bs-link-color:{}", low));
                parts.push(format!("--bs-link-color-rgb:{},{},{}", r, g, b));
            } else if *key == "accent" {
                let (r, g, b) = rgb_channels(&low);
                parts.push(format!("--bs-link-hover-color:{}", low));
                parts.push(format!("--bs-link-hover-color-rgb:{},{},{}", r, g, b));
            }
        }
    }
    let stock_tint = tint("#4154f1");
    if let Some(t) = brand["primary_tint"].as_str() {
        if !t.is_empty() && t != stock_tint && valid_color(t) {
            parts.push(format!("--brand-primary-tint:{}", t));
        }
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(":root{{{}}}", parts.join(";"))
    }
}

fn detect_image_type(head: &[u8]) -> Option<&'static str> {
    if head.len() >= 8 && head[..8] == [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A] {
        Some("png")
    } else if head.len() >= 3 && head[..3] == [0xFF, 0xD8, 0xFF] {
        Some("jpg")
    } else if head.len() >= 6 && (&head[..6] == b"GIF87a" || &head[..6] == b"GIF89a") {
        Some("gif")
    } else {
        None
    }
}

async fn put_setting(db: &Db, name: &str, value: &str) -> Result<(), String> {
    let val = value.trim();
    if name.starts_with("color_") && !val.is_empty() && !valid_color(val) {
        return Err(format!("{} is not a hex colour like #1a2b3c", val));
    }
    let existing = db
        .first("select id from portal_setting where module='branding' and name=:n limit 1", &row! {"n"=>name})
        .await
        .ok()
        .flatten();
    if val.is_empty() {
        if let Some(r) = existing {
            if let Some(id) = i(&r, "id") {
                let _ = db.delete("portal_setting", id).await;
            }
        }
    } else if let Some(r) = existing {
        if let Some(id) = i(&r, "id") {
            let _ = db.update("portal_setting", id, &row! {"text"=>val}).await;
        }
    } else {
        let _ = db
            .insert("portal_setting", &row! {"module"=>"branding","name"=>name,"type"=>"Text","text"=>val})
            .await;
    }
    Ok(())
}

async fn remove_image_link(db: &Db, slug: &str, setting_key: &str) {
    if let Ok(Some(f)) = db.first("select id from file_link where slug=:s limit 1", &row! {"s"=>slug}).await {
        if let Some(id) = i(&f, "id") {
            crate::files::delete_file_link(db, id).await;
        }
    }
    let _ = put_setting(db, setting_key, "").await;
}

async fn handle_image(ctx: &Ctx, field: &str, slug: &str, setting_key: &str) -> Result<(), String> {
    if !ctx.p(&format!("remove_{}", field)).is_empty() {
        remove_image_link(ctx.db(), slug, setting_key).await;
        return Ok(());
    }
    let Some(up) = ctx.files.get(field) else { return Ok(()) };
    if up.size == 0 {
        return Ok(());
    }
    if up.size > MAX_IMAGE_BYTES {
        return Err(format!("{} rejected: exceeds 2 MB limit", field));
    }
    let Some(ext) = detect_image_type(up.head()) else {
        return Err(format!("{} rejected: only PNG, JPG or GIF images are allowed", field));
    };
    let dir = std::path::PathBuf::from(&ctx.st.cfg.upload_dir).join("portal");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let target = dir.join(format!("{}.{}", slug, ext));
    let target_str = target.to_string_lossy().to_string();

    let existing = ctx.db().first("select id, path from file_link where slug=:s limit 1", &row! {"s"=>slug}).await.ok().flatten();
    if let Some(ref prev) = existing {
        let prev_path = s(prev, "path");
        if !prev_path.is_empty() && prev_path != target_str {
            let _ = tokio::fs::remove_file(&prev_path).await;
        }
    }
    up.save_to(&target).await.map_err(|e| e.to_string())?;
    let clean_name = if up.filename.is_empty() { format!("{}.{}", slug, ext) } else { up.filename.clone() };
    if let Some(prev) = existing {
        if let Some(id) = i(&prev, "id") {
            let _ = ctx
                .db()
                .update("file_link", id, &row! {"name"=>clean_name,"path"=>target_str,"size"=>up.size as i64,"allowedroles"=>"All"})
                .await;
        }
    } else {
        let _ = ctx
            .db()
            .insert("file_link", &row! {"module"=>"portal","slug"=>slug,"name"=>clean_name,"path"=>target_str,"size"=>up.size as i64,"allowedroles"=>"All"})
            .await;
    }
    put_setting(ctx.db(), setting_key, slug).await?;
    Ok(())
}

pub async fn action(mut ctx: Ctx, act: &str) -> Response {
    if ctx.user.is_none() {
        return ctx.need_login().await;
    }
    if !ctx.is_admin() {
        return ctx.status(StatusCode::FORBIDDEN, "Superuser access required").await;
    }
    match act {
        "save" => {
            if ctx.method != "POST" {
                return ctx.redirect("/branding/index").await;
            }
            let mut err: Option<String> = None;
            for key in ["app_name", "copyright", "team", "homepage", "logo_url"] {
                if let Err(e) = put_setting(ctx.db(), key, &ctx.p(key)).await {
                    err = Some(e);
                    break;
                }
            }
            if err.is_none() {
                for (key, _, _, _) in COLOR_DEFAULTS {
                    let field = format!("color_{}", key);
                    if let Err(e) = put_setting(ctx.db(), &field, &ctx.p(&field)).await {
                        err = Some(e);
                        break;
                    }
                }
            }
            if err.is_none() {
                if let Err(e) = handle_image(&ctx, "logo", "branding_logo", "logo_slug").await {
                    err = Some(e);
                }
            }
            if err.is_none() {
                if let Err(e) = handle_image(&ctx, "favicon", "branding_favicon", "favicon_slug").await {
                    err = Some(e);
                }
            }
            crate::cache::SETTINGS.clear();
            match err {
                Some(e) => ctx.flash(&e),
                None => ctx.flash("Branding updated"),
            }
            ctx.redirect("/branding/index").await
        }
        "reset" => {
            if ctx.method != "POST" {
                return ctx.redirect("/branding/index").await;
            }
            let _ = ctx.db().exec("delete from portal_setting where module='branding'", &db::no_params()).await;
            for slug in ["branding_logo", "branding_favicon"] {
                if let Ok(Some(f)) = ctx.db().first("select id from file_link where slug=:s limit 1", &row! {"s"=>slug}).await {
                    if let Some(id) = i(&f, "id") {
                        crate::files::delete_file_link(ctx.db(), id).await;
                    }
                }
            }
            crate::cache::SETTINGS.clear();
            ctx.flash("Branding reset to defaults");
            ctx.redirect("/branding/index").await
        }
        _ => index(ctx).await,
    }
}

async fn index(mut ctx: Ctx) -> Response {
    let brand = web::branding(&ctx.st).await;
    let csrf = ctx.csrf_field();
    let app_name = esc(brand["app_name"].as_str().unwrap_or(""));
    let copyright = esc(brand["copyright"].as_str().unwrap_or(""));
    let team = esc(brand["team"].as_str().unwrap_or(""));
    let homepage = esc(brand["homepage"].as_str().unwrap_or(""));
    let logo_url = esc(brand["logo_url"].as_str().unwrap_or(""));
    let cur_logo = esc(brand["logo"].as_str().unwrap_or("/static/images/logo.png"));
    let cur_favicon = esc(brand["favicon"].as_str().unwrap_or("/static/images/favicon.svg"));
    let has_logo_slug = !brand["logo_slug"].as_str().unwrap_or("").is_empty();
    let has_favicon_slug = !brand["favicon_slug"].as_str().unwrap_or("").is_empty();

    let logo_block = if has_logo_slug {
        format!(
            r#"<p class="mb-2"><img src="{cur_logo}" alt="Current logo" style="max-height:30px"/></p>
              <div class="form-check mb-2">
                <input class="form-check-input" type="checkbox" name="remove_logo" id="remove_logo" value="1"/>
                <label class="form-check-label" for="remove_logo">Remove current logo</label>
              </div>"#
        )
    } else {
        String::new()
    };
    let favicon_block = if has_favicon_slug {
        format!(
            r#"<p class="mb-2"><img src="{cur_favicon}" alt="Current favicon" style="max-height:24px"/></p>
              <div class="form-check mb-2">
                <input class="form-check-input" type="checkbox" name="remove_favicon" id="remove_favicon" value="1"/>
                <label class="form-check-label" for="remove_favicon">Remove current favicon</label>
              </div>"#
        )
    } else {
        String::new()
    };

    let mut colors_html = String::new();
    for (key, fallback, label, hint) in COLOR_DEFAULTS {
        let val = esc(brand[key].as_str().unwrap_or(fallback));
        colors_html.push_str(&format!(
            r##"<div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="color_{key}">{label}</label>
          <div class="col-sm-8">
            <div class="brand-swatch">
              <input type="color" id="color_{key}" value="{val}" data-brand-key="{key}"/>
              <input type="text" class="form-control" name="color_{key}" value="{val}" pattern="#[0-9a-fA-F]{{6}}" data-brand-text="{key}"/>
            </div>
            <p class="brand-hint">{hint} Default {fallback}.</p>
          </div>
        </div>"##
        ));
    }

    let body = format!(
        r##"<style>
  .brand-swatch {{ display:flex; align-items:center; gap:10px; }}
  .brand-swatch input[type=color] {{ width:46px; height:38px; padding:2px; border:1px solid #ced4da; border-radius:4px; background:#fff; }}
  .brand-swatch input[type=text] {{ max-width:130px; font-family:monospace; }}
  .brand-hint {{ font-size:12px; color:#899bbd; margin-bottom:0; }}
  .brand-preview {{ border:1px solid #ebeef4; border-radius:6px; overflow:hidden; }}
  .brand-preview .pv-header {{ height:46px; display:flex; align-items:center; padding:0 14px; font-weight:700; box-shadow:0 2px 10px rgba(1,41,112,.08); }}
  .brand-preview .pv-body {{ display:flex; min-height:150px; }}
  .brand-preview .pv-side {{ width:38%; padding:12px; }}
  .brand-preview .pv-main {{ flex:1; padding:14px; }}
  .brand-preview .pv-nav {{ display:block; padding:8px 10px; border-radius:4px; font-size:13px; font-weight:600; margin-bottom:6px; }}
  .brand-preview .pv-btn {{ display:inline-block; padding:6px 14px; border-radius:4px; color:#fff; font-size:13px; }}
  .brand-preview .pv-logo {{ max-height:22px; margin-right:8px; }}
</style>
<div class="pagetitle">
  <h1>Branding</h1>
  <nav>
    <ol class="breadcrumb">
      <li class="breadcrumb-item"><a href="/">Home</a></li>
      <li class="breadcrumb-item active">Branding</li>
    </ol>
  </nav>
</div>
<div class="row">
  <div class="col-lg-7">
    <form action="/branding/save" method="post" enctype="multipart/form-data">
    {csrf}
    <div class="card">
      <div class="card-body">
        <h5 class="card-title">Identity</h5>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="app_name">Portal name</label>
          <div class="col-sm-8">
            <input type="text" class="form-control" id="app_name" name="app_name" value="{app_name}"/>
            <p class="brand-hint">Shown in the browser tab and beside the logo.</p>
          </div>
        </div>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="logo">Logo</label>
          <div class="col-sm-8">
            {logo_block}
            <input type="file" class="form-control" id="logo" name="logo" accept="image/png,image/jpeg,image/gif"/>
            <p class="brand-hint">PNG, JPG or GIF, up to 2&nbsp;MB. Displays at 26&nbsp;px tall, so a wide logo works best.</p>
          </div>
        </div>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="logo_url">Logo URL</label>
          <div class="col-sm-8">
            <input type="text" class="form-control" id="logo_url" name="logo_url" value="{logo_url}"/>
            <p class="brand-hint">Only used when no logo file is uploaded above.</p>
          </div>
        </div>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="favicon">Favicon</label>
          <div class="col-sm-8">
            {favicon_block}
            <input type="file" class="form-control" id="favicon" name="favicon" accept="image/png,image/jpeg,image/gif"/>
            <p class="brand-hint">A square PNG of 32&times;32 or 96&times;96 works best.</p>
          </div>
        </div>

        <h5 class="card-title">Footer</h5>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="copyright">Copyright holder</label>
          <div class="col-sm-8">
            <input type="text" class="form-control" id="copyright" name="copyright" value="{copyright}"/>
          </div>
        </div>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="team">Credit line</label>
          <div class="col-sm-8">
            <input type="text" class="form-control" id="team" name="team" value="{team}"/>
          </div>
        </div>
        <div class="row mb-3">
          <label class="col-sm-4 col-form-label" for="homepage">Credit link</label>
          <div class="col-sm-8">
            <input type="text" class="form-control" id="homepage" name="homepage" value="{homepage}"/>
          </div>
        </div>

        <h5 class="card-title">Colours</h5>
        {colors_html}

        <button type="submit" class="btn btn-primary">Save branding</button>
      </div>
    </div>
    </form>
  </div>

  <div class="col-lg-5">
    <div class="card">
      <div class="card-body">
        <h5 class="card-title">Preview</h5>
        <div class="brand-preview" id="brandPreview">
          <div class="pv-header">
            <img src="{cur_logo}" class="pv-logo" alt=""/>
            <span data-pv="name">{app_name}</span>
          </div>
          <div class="pv-body">
            <div class="pv-side">
              <span class="pv-nav" data-pv="nav-active">Dashboard</span>
              <span class="pv-nav" data-pv="nav">Reports</span>
              <span class="pv-nav" data-pv="nav">Settings</span>
            </div>
            <div class="pv-main">
              <h6 data-pv="heading">Page title</h6>
              <p style="font-size:13px">Body text with a <a href="#" data-pv="link" onclick="return false">link</a> in it.</p>
              <span class="pv-btn" data-pv="btn">Button</span>
            </div>
          </div>
        </div>
        <p class="brand-hint mt-2">Updates as you pick colours. Save to apply across the portal.</p>
      </div>
    </div>
    <div class="card">
      <div class="card-body">
        <h5 class="card-title">Start over</h5>
        <p class="brand-hint mb-3">Clears every branding value and returns the portal to its shipped look.</p>
        <form action="/branding/reset" method="post">
          {csrf}
          <button type="submit" class="btn btn-outline-secondary btn-sm">Reset to defaults</button>
        </form>
      </div>
    </div>
  </div>
</div>
<script>
(function () {{
  var preview = document.getElementById('brandPreview');
  if (!preview) return;
  function paint() {{
    function val(k) {{
      var el = document.querySelector('[data-brand-text="' + k + '"]');
      return el ? el.value : '';
    }}
    var primary = val('primary'), heading = val('heading');
    var pageBg = val('page_bg'), headerBg = val('header_bg'), sidebarBg = val('sidebar_bg');
    preview.querySelector('.pv-header').style.background = headerBg;
    preview.querySelector('.pv-header').style.color = heading;
    preview.querySelector('.pv-side').style.background = sidebarBg;
    preview.querySelector('.pv-main').style.background = pageBg;
    preview.querySelector('[data-pv="heading"]').style.color = heading;
    preview.querySelector('[data-pv="link"]').style.color = primary;
    var pvBtn = preview.querySelector('[data-pv="btn"]');
    pvBtn.style.background = primary;
    if (/^#[0-9a-fA-F]{{6}}$/.test(primary)) {{
      var r = parseInt(primary.slice(1, 3), 16), g = parseInt(primary.slice(3, 5), 16), b = parseInt(primary.slice(5, 7), 16);
      pvBtn.style.color = (0.299 * r + 0.587 * g + 0.114 * b > 186) ? '#212529' : '#ffffff';
    }}
    var active = preview.querySelector('[data-pv="nav-active"]');
    active.style.background = pageBg;
    active.style.color = primary;
    Array.prototype.forEach.call(preview.querySelectorAll('[data-pv="nav"]'), function (el) {{
      el.style.background = sidebarBg;
      el.style.color = heading;
    }});
  }}
  Array.prototype.forEach.call(document.querySelectorAll('[data-brand-key]'), function (well) {{
    var key = well.getAttribute('data-brand-key');
    var text = document.querySelector('[data-brand-text="' + key + '"]');
    well.addEventListener('input', function () {{ text.value = well.value; paint(); }});
    text.addEventListener('input', function () {{
      if (/^#[0-9a-fA-F]{{6}}$/.test(text.value)) {{ well.value = text.value; paint(); }}
    }});
  }});
  var nameField = document.getElementById('app_name');
  if (nameField) {{
    nameField.addEventListener('input', function () {{
      preview.querySelector('[data-pv="name"]').textContent = nameField.value;
    }});
  }}
  paint();
}})();
</script>"##
    );
    ctx.html("Branding", body).await
}

/// Exposed for tests.
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn color_validation_and_tint() {
        assert!(valid_color("#4154f1"));
        assert!(valid_color("#fff"));
        assert!(!valid_color("red"));
        assert!(!valid_color("#4154f1; } body{display:none"));
        assert_eq!(tint("#4154f1"), "#f0f1fe");
        let stock = json!({"primary":"#4154f1","accent":"#717ff5","heading":"#012970","page_bg":"#f6f9ff","header_bg":"#ffffff","sidebar_bg":"#ffffff","primary_tint":"#f0f1fe"});
        assert_eq!(css_variables(&stock), "");
        let custom = json!({"primary":"#1b5e20","accent":"#717ff5","heading":"#012970","page_bg":"#f6f9ff","header_bg":"#ffffff","sidebar_bg":"#ffffff","primary_tint":tint("#1b5e20")});
        let css = css_variables(&custom);
        assert!(css.contains("--brand-primary:#1b5e20"));
        assert!(css.contains("--brand-primary-hover:"));
        assert!(css.contains("--brand-primary-active:"));
        assert!(css.contains("--brand-primary-rgb:27,94,32"));
        assert!(css.contains("--brand-primary-fg:#ffffff"));
        assert!(css.contains("--bs-primary:#1b5e20"));
        assert!(css.contains("--brand-primary-tint:"));
        assert_eq!(contrast_fg("#ffd54f"), "#212529");
        assert_eq!(contrast_fg("#1b5e20"), "#ffffff");
    }
}
