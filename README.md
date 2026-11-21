# rustportal

A portal framework for workflow and web systems, written in Rust: dynamic pages
stored in the database, tracker modules with workflows, and the admin tools around
them. Modules written for Grails/GSP-based portals can be imported and translated.

Stack: axum, PostgreSQL (sqlx), Rhai for scripts, MiniJinja for templates.

## Run

    createdb rustportal            # once
    cp config.example.toml config.toml
    cargo run --release

On first start a setup token is printed (and written to `setup-token.txt`);
open `/setup` to create the first administrator, then log in at `/login`.
Tables are created and extended automatically at start-up.

## URLs

| URL | Purpose |
|---|---|
| `/view/{module}/{slug}[/arg1/arg2/arg3]` | render a page template |
| `/run/{module}/{slug}` | run a runable page (script) |
| `/{module}/{slug}/list`, `/create`, `/display/{id}` | tracker list, new record, record |
| `/{module}/{slug}/{transition}[/{id}]` | transition form (GET) and submit (POST) |
| `/{module}/{slug}/updaterecord/{id}` | add an audit-trail update |
| `/download/{module}/{slug}` | file link download |
| `/svc/{module}/{slug}/...` | module endpoint (CGI or proxy) |
| `/{controller}/{index,show,create,edit,...}` | admin screens (`portalTracker`, `portalPage`, `user`, ...) |

Custom tracker screens are pages named `{slug}_list`, `{slug}_show_default`,
`{slug}_show_{status}`, `{slug}_{transition}` and `{slug}_edit`.

## Dynamic page language

Pages are written in **Rhai** (scripts) and **MiniJinja** (templates).

| Where | Language |
|---|---|
| page content (normal page), e-mail body pages, page titles | MiniJinja (HTML-escaped; use `\|safe` for raw HTML) |
| page preprocess (result is `pp`), runable page content, transition postprocess, status runonupdate | Rhai |
| field options / default / error function / error message, transition enabled condition and update trail, row class, e-mail to/cc | Rhai expression |
| "Data Compare" role rule, page data source query | MiniJinja producing SQL (use `\|sql` to quote values) |

Variables: `params`, `curuser` (`id`, `name`, `userid`, `email`, `is_admin`, ...),
`datas`; plus `pp`, `tracker`, `transition`, `userroles`, `flash` in templates and
`trail_id`, `prevdata`, `transition`, `tracker` in postprocess scripts.

### Script functions (Rhai)

    rows(module, slug [, qparams [, order [, offset, limit]]])   first_row(...)   get_data(module, slug, id)
    count_rows(module, slug, qparams)   save_data(module, slug, map) -> id   delete_data(module, slug, id)
    save_trail(module, slug, id, text [, status])   update_trail_desc(module, slug, trail_id, text)
    sql_rows(sql [, params])   sql_first(...)   sql_exec(...)          // :name placeholders
    setting(name [, default])   user(id)   user_by_staffid(s)   users_with_role(module, role)
    module_roles(module)   has_role(module, role)   tracker_roles(module, slug [, datas])
    page_allowed(module, slug)   may the user open that page (() = no such page)
    send_email(#{to, cc, subject, body})   run_page(module, slug [, vars])   render_page(module, slug [, vars])
    read_excel(file_id [, sheet])   save_upload(field, module)   file_info(id)
    delete_file(id)   upload_info(field)   save_upload_as(field, #{dir, filename, module, slug, tracker_id, tracker_data_id})
    tree_ensure(module, name)   tree_add(parent, name [, slug [, data]])   tree_children(node)   tree_find_node(tree_id, name)
    tree_delete_node(id)   tree_clear(tree_id)   tree_count(tree_id)
    now()  today()  date_add(date, days)  date_format(date, fmt)  days_between(a, b)  format_number(v, decimals)
    now_ms()  epoch_ms(datetime)  uuid()  random_int(n)  shuffle(list)  shuffle_seeded(list, seed)
    session_get(key)  session_set(key, value)   request_base()   upload_text(field)   csrf_token()  csrf_field()
    regex_match(p, s)  regex_replace(p, s, rep)  regex_captures(p, s)  json_parse(s)  json_string(v)  json_pretty(v)
    markdown_to_html(md)   nfd(text)   levenshtein(a, b)

`shuffle_seeded` gives the same order as Java's `Collections.shuffle(list, new Random(seed))`,
so pages ported from a Java/Groovy portal keep orders shuffled there. `session_*` cannot touch the portal's
own keys (userid, csrf, flash). Templates also get `session_get(key)` and `request_base()`.

qparams is a small query DSL: `#{status: "Open"}`, `"%like%"`, `">5"`, `"<=2026-01-01"`,
`"!Closed"`, lists (any of), `()` for null, and nested `and` / `or` / `not` maps.

A runable page returns a map: `flash`, `redirect_slug` / `redirect_module` /
`redirect_params`, `redirect_url`, or `content`. With render `JSON` the result is
sent as JSON; with `XLSX` return `#{ filename, header: [...], rows: [...] }`
(or `sheets: [...]`; a sheet may also have `widths`, `freeze: false`, `bold_header: false`, cells written
as `#{v, bold: true}` or `#{v, date: true}`, and `charts: [#{title, series, first_row, last_row,
cat_col, val_col, horizontal, row, col, height_rows}]` for native bar charts over its cells);
with `File` return a file id or `#{ content, filename }`.

`rustportal module-export <module>` and `rustportal module-import <module> [dir]` do the
Module screen's export and import from the command line (`CONFIG=path` for another config).

### Template functions (MiniJinja)

    {{ tracker_nav() }} {{ tracker_filter() }} {{ tracker_list() }}          {# current tracker, or (module, slug) #}
    {{ tracker_display() }} {{ tracker_updates() }} {{ transition_buttons() }} {{ tracker_transitions() }}
    {{ tracker_info() }} {{ tracker_form() }}                                {# transition=, id= #}
    {{ display_field(module, slug, field, value, datas) }}
    rows(), first_row(), get_data(), sql_rows(), sql_first(), setting(), user(), user_name(id),
    has_role(), module_roles(), tracker_roles(), transition_allowed(module, slug, name, datas),
    include_page(module, slug, k=v), run_page(module, slug, k=v), script("rhai code"),
    page_url(), run_url(), list_url(), create_url(), display_url(), transition_url(), download_url(), file_url(),
    csrf_field(), csrf_token(), now(), today()
    filters: format_number(decimals), format_date("%d/%m/%Y"), sql, nl2br

Every POST needs the session token: forms rendered inside the layout get it
automatically; full-page templates must include `{{ csrf_field() }}`.

## Modules

Admin -> Module exports a module to `modulemigration/{module}/`
(`trackerlist.json`, `pagelist.json`, `settinglist.json`, `pages/content_*.html|rhai`,
`pages/pp_*.rhai`) and imports it back; zips can be downloaded and uploaded.
The JSON layout is the one Grails/GSP portals use, so their tracker definitions
import as-is — but page sources and expressions written in Groovy/GSP must be ported.

## Sharing a database with a Grails portal

Set `page_table` / `page_data_table` in `config.toml` (e.g. `rp_portal_page` /
`rp_portal_page_data`) to keep this portal's pages in their own tables while every
other table - users, trackers and their data, settings, files - is shared with the
Grails portal on the same database. Tracker definitions refer to pages by id
(post-process, run-on-update and e-mail body pages), so pages copied across keep
their ids; on such a database new tables take ids from `hibernate_sequence`, so
pages added on either side never collide.

For a g6 Grails portal also set `g6_schema = true` (enables g6 runtime
compatibility for GSP role rules, raw HTML field values, and file links;
`portal_user` and `portal_tree*` tables are used directly),
`run_scheduler = false` when the Grails portal already runs the schedules, and
`content_security_policy` to the policy the Grails portal sends, so pages that
load outside fonts or images look the same on both, and `user_profile` to its
`server.user_profile` (e.g. `"ahli.ahli"`): the tracker holding each user's member
record. That record is `session['profile']` in pages and role rules.

In g6 mode a tracker role rule is read as the Grails portal's GSP template: its
`${...}` pieces (with `curuser`, `datas`, `session['profile']`,
`g6portal.PortalSetting.namedefault(...)` and `array2sql(...)`) are evaluated, and
a rule that fails grants nothing. List and display values print as stored, HTML
included, unless the field has `encode_exception` set, as the Grails portal does.
A page's allowed roles do not let a superuser (`is_admin`) in by themselves there:
as in the Grails portal, only through the module roles `enablesuperuser` grants.

The Grails portal keeps users, roles, settings, trackers and trees in
Hibernate's second-level cache, so a change made here only shows there once its
cache entry expires. Keep that cache short (g6ikram's `ehcache.xml` expires
entries after 60 seconds, as this portal's own cache does).

## Importing Grails/GSP modules

Upload the Grails portal's module zip with Module -> Import Zip, the same as a
zip exported here. A Grails zip is recognised by its layout (`pagelist.json`,
`trackerlist.json` and `pages/` at the top of the zip, no module folder) and the
module is named after the zip file, as the Grails portal does.

What the import does with the page sources (`pages/*.gsp`):

| Source | Result |
|---|---|
| template page (not runable) | translated from GSP to MiniJinja (`src/gsp.rs`) |
| page data source query | the Groovy expression is translated to a MiniJinja template |
| runable page | kept as commented-out Groovy; running it stops with "not ported" |
| preprocess script | kept as commented-out Groovy; the page renders without `pp` |

Anything in a template that could not be translated is left in place as a
`{# GSP-TODO ... #}` comment, and every page that needs a look is listed in
the import log (Module page -> Import History -> View).

To see how well a module translates without importing it:

    cargo run -- gsp-check /path/to/modulemigration                     # all modules, summary
    cargo run -- gsp-check /path/to/modulemigration/kpi --show          # every warning
    cargo run -- gsp-check /path/to/modulemigration/kpi/pages/content_x.gsp   # print one translation

Translated templates keep Groovy method calls (`x.toInteger()`, `s.trim()`,
`list.size()`, `d.format('dd/MM/yyyy')`): the template engine implements them
(`src/groovy.rs`), null-safe. Closures become `g_closure(list, "findAll", "it",
"<expression>", {captured variables})`, links become `grails_url(controller=,
action=, params=)`, and variables changed inside a loop live in `ns`.

Not translated: Groovy in tracker definitions (field defaults/queries, role
rules, enabled conditions, e-mail recipients), multi-statement closures, and
SQL written for SQL Server.

## Caching

Tracker definitions, pages (and their data sources), settings, menus (trees) and
each user's account and roles are kept in memory (`src/cache.rs`,
`src/tracker.rs`). Every statement goes through `db.rs`, and a write to a table a
cache is built from clears that cache, so changes made through this server -
admin screens, module import, scripts running SQL - show up on the next request.
Changes made by another server or directly in the database show up within 60
seconds. With several servers behind a load balancer, expect up to a minute of
lag for definition changes made on another server.

`RUSTPORTAL_LOG_SQL=1` prints every statement to stderr, which is the quickest way
to see what a slow page is doing.

## Running on a copy of a Grails portal's database

rustportal can run on a copy of a Grails/GSP portal's Postgres database with the
same table and column names. At startup it adds missing columns and gives those tables
defaults for `id` (from Hibernate's `hibernate_sequence`, so ids never collide if
both applications use the database) and `version`. Translate the modules' GSP
pages in place with `rustportal translate-pages <module>...`.

## Excel upload and download

Tracker data upload (`/portalTrackerData/upload?tracker_id=...`) and the list's
Excel download are built for large files (tested with 580k rows x 130 columns,
a 674 MB .xlsx):

- uploads over 8 MB are streamed to a temporary file as they arrive, never held in
  memory whole;
- the import runs in the background (the upload record shows "Upload is currently
  in queue", progress every 20,000 rows, then the result); .xlsx sheets are read
  cell by cell and new records inserted 500 per statement (a batch the database
  rejects is retried row by row, so only the bad rows are rejected, each with its
  message); the job is logged in `portal_background_trail` as
  `portalTrackerData.upload`;
- header cells are matched to field names or labels ignoring case and punctuation;
  a column headed `id` updates the record with that id, unless the upload form's
  "A column headed id" is set to "is ignored" (for files exported from another
  system that carry their own ids);
- cell text is trimmed, empty cells are stored as NULL, whole numbers are stored
  without ".0";
- the Excel download reads the list query through a database cursor 2,000 rows at
  a time and streams the rows into the .xlsx (`src/xlsxstream.rs`; a new sheet
  every 1,048,575 rows), and the finished file is streamed to the browser.

## Audit trail

Every request is recorded in `portal_audit_trail`, but requests never wait for it:
rows are queued in memory and written by a background task every 2 seconds, up to
500 rows per insert (`src/audit.rs`). A normal
shutdown (Ctrl-C or SIGTERM) writes out the queue; a crash can lose the last 2
seconds. If the database falls behind and 20,000 rows are waiting, new rows are
dropped and the count is written to the error log.
