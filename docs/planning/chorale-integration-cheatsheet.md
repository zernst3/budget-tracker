# chorale v0.2.0 Integration Cheat-Sheet — Master/Detail + Grouping + In-Cell Editing

**Audience:** build agents wiring the budget-tracker ledger + Pending UIs (SPEC §7).
**Scope:** the three chorale v0.2.0 features the ledger needs — (1) master/detail
sub-tables, (2) grouping + aggregation, (3) in-cell editing.

All API cites are into **chorale `main` @ `2862159`** (`2862159...` =
`28621596424d8fc485eea44577955f8adc853758`), which is exactly what
`budget-ui` compiles against (`Cargo.lock` pins
`git+https://github.com/zernst3/rust-chorale?branch=main#2862159...`).

The full snippet at the bottom is **compile-verified** — see the marked block.

---

## 0. Crate map / imports

Everything comes from two crates, both already deps of `budget-ui`:

```rust
use chorale_core::{
    AggregatorKind, Alignment, CellValue, ColumnDef, ColumnId, CommittedEdit,
    CurrencyCode, EditorKind, RenderKind, RowId, TableState,
    // also available: GroupKey, GroupedPaginationMode, SortAction, FilterKind,
    // FrozenSide, PriorEdit, Labels, EditTarget, NaiveDate (re-exported chrono)
};
use chorale_dioxus::{use_table, Table, UseTableHandle};
use dioxus::prelude::*;
```

Re-export cites: `chorale-core/src/lib.rs` — `ColumnDef` L72, `EditorKind` L78,
`AggregatorKind` L574, `CommittedEdit` L158, `GroupKey` L585,
`GroupedPaginationMode` L579, `SortAction` L173, `RowId` L115.
`chorale-dioxus/src/lib.rs` — `Table` L71, `use_table` L127, `UseTableHandle` L144.

### Rows + state

- `use_table(init: impl Fn() -> TableState<TRow>) -> UseTableHandle<TRow>`
  (`chorale-dioxus/src/hooks.rs:298`). `UseTableHandle` is `Copy`
  (`hooks.rs:31`) — capture it freely in move-closures.
- `TableState::new(rows: Vec<(RowId, TRow)>, columns: Vec<ColumnDef<TRow>>)`
  (`chorale-core/src/state.rs:229`). **You assign each row's `RowId` yourself**
  (`RowId::new()` — `types.rs:16`) and pass `(RowId, TRow)` pairs. RowId is a
  newtype over `Uuid`; it must be **stable for the row's lifetime** — selection,
  expansion, editing, and grouping all track rows by `RowId`. Re-generating
  RowIds on every render breaks expansion/selection persistence.
- `TableState` public fields you set directly in the `init` closure (all `pub`,
  `chorale-core/src/state.rs`): `rows` L44, `columns` L46, `page_size` L58,
  `grouping` L105, `collapsed_groups` L110, `expanded_rows` L114,
  `editing` L74, `row_heights` L81.

---

## 1. MASTER / DETAIL sub-tables (CHANGELOG Item 12)

**Goal:** parent `<Table>` of DAY rows; each row expands to a child `<Table>` of
that day's transactions.

### The prop

```rust
// chorale-dioxus/src/components.rs:294
detail_renderer: Option<Callback<TRow, Element>>   // #[props(default)] -> optional
```

- It is a **`dioxus::Callback<TRow, Element>`**, NOT `EventHandler`. (The
  CHANGELOG prose says `EventHandler<TRow, Element>` — that is **wrong**; the
  real prop is `Callback`. Build with `Callback::new(move |row: TRow| rsx!{…})`.)
- When `Some`, chorale prepends a **24px chevron column at index 0**
  (`components.rs:251-255` doc; chevron click calls `toggle_row_expansion`,
  `hooks.rs:245`). The detail panel renders as a **full-width
  `<tr><td colspan>`** directly under the parent row
  (`detail_panel_tr`, `components.rs:1399 / 1570`).
- The callback receives the **parent row by value** (`TRow`), so the child table
  can be built from it. Mount **any `Element`** inside — for the nested-grid use
  case, mount a child `<Table>` component.
- Expansion state lives in `TableState.expanded_rows: HashSet<RowId>`
  (`state.rs:114`); transitions `toggle_row_expansion` / `collapse_all_rows`
  (`transitions` re-exported `lib.rs:408,411`; handle methods `hooks.rs:245,250`).
- Variable-height: setting `detail_renderer` auto-enables row-height measurement
  for the parent (`components.rs:333-340` — `has_detail` path); you do **not**
  need to also pass `variable_row_height: true`.

### Child table inside the detail panel — use `inline: true`

```rust
// child Table prop, components.rs:235
inline: bool   // default false
```

`inline: true` makes the child render at **natural full height with NO internal
scroll container and NO virtualization** (all rows in one batch). This is the
correct mode for a table embedded inside the parent's scroll viewport — it avoids
nested-scroll wheel hand-off discontinuities. Keep the child dataset small
(<~500 rows; a day's transactions easily qualify). Set the child's
`page_size` high enough that it never paginates (e.g. `s.page_size = n.max(1)`).

> Canonical usage: `examples/qa-harness/src/main.rs` — `EmployeeDetailPanel`
> (L96-139, child `Table` with `inline: true`) and the parent `detail_renderer`
> prop (L776-779: `Some(Callback::new(|employee: Employee| rsx!{…}))`).

**Budget mapping:** parent = day-ledger table (one row per day). Child =
that day's transactions, `inline: true`.

---

## 2. GROUPING + AGGREGATION (CHANGELOG Item 8)

**Goal:** group the child transaction table by expense CATEGORY, with a
per-category subtotal.

### Turn grouping on

Two equivalent paths:

- **In the `init` closure** (static initial grouping): set the public field
  `s.grouping = vec![ColumnId("category")];` (`state.rs:105`).
- **Reactively** (toggle button): `handle.set_grouping(vec![ColumnId("category")])`
  (`hooks.rs:222`); `handle.set_grouping(vec![])` clears it.
  Related handle methods: `toggle_group(GroupKey)` (`hooks.rs:230`),
  `expand_all_groups` (L235), `collapse_all_groups` (L240).

`grouping` is a `Vec<ColumnId>` — multi-level grouping is supported; pass a
single-element vec for one level.

### Aggregation — on the ColumnDef

```rust
// chorale-core/src/column.rs:390 — builder method
ColumnDef::new(ColumnId("amount"), "Amount", |t: &Txn| CellValue::Float(t.amount))
    .aggregator(AggregatorKind::Sum)
```

`AggregatorKind<TRow>` (`column.rs:16`, `#[non_exhaustive]`):
`Sum`, `Average`, `Count`, `Min`, `Max`, `Custom(Arc<dyn Fn(&[&TRow]) -> CellValue>)`.
The aggregated value appears in each group header's `aggregates` vec at that
column's position (`column.rs:6-11`). Aggregation only fires when grouping is
active (`state.grouping` non-empty).

- `Sum`/`Average` operate on `CellValue::Integer` and `Float`. **Budget money is
  `rust_decimal::Decimal`** — your accessor must return `CellValue::Float(...)`
  (or `Integer`) for `Sum` to aggregate it. There is no `Decimal` CellValue
  variant (`CellValue`, `types.rs:104`: Text/Integer/Float/Boolean/Date/DateTime/Empty).
  Convert Decimal→f64 at the accessor boundary **for display/aggregation only**;
  keep the Decimal as the source of truth elsewhere (BUDGET-MONEY-1).

### Group-header styling

```rust
// components.rs:322 — every group-header <tr> gets this class
group_header_class: String   // default "chorale-group-header"
```

### Grouped pagination mode (optional)

`TableState.grouped_pagination_mode` / `GroupedPaginationMode`
(`DataRowsOnly` vs `Virtualized`) controls how grouped rows paginate
(`types.rs`, re-export `lib.rs:579`). Default is fine for small child tables;
with `inline: true` it is not load-bearing.

**Budget mapping:** child transaction table, `grouping = [ColumnId("category")]`,
`amount` column carries `.aggregator(AggregatorKind::Sum)` for the per-category
subtotal.

---

## 3. IN-CELL EDITING (CHANGELOG Item 7)

**Goal:** exactly two editable columns on transaction rows — `category` and
`comment` — everything else read-only.

### Make a column editable — `.editor(...)` on the ColumnDef

```rust
// chorale-core/src/column.rs:372 — builder method
ColumnDef::new(ColumnId("comment"), "Comment", accessor).editor(EditorKind::Text)
```

`EditorKind` (`column.rs:198`, `#[non_exhaustive]`):
- `Text` → `<input type="text">`
- `Number { min, max, step }` → `<input type="number">`
- `Date` → `<input type="date">`
- `BoolToggle` → `<input type="checkbox">`
- `Custom` → **still renders a plain text `<input>`** in the built-in editor
  (see gotcha below).

A column **without** `.editor(...)` is read-only — `editor: None` is the default
(`column.rs:278,311`) and `start_edit` returns `Err(ColumnNotEditable)` for it
(`column.rs:368-374`). So the `amount`/`date` columns stay read-only simply by
omitting `.editor(...)`.

### Commit callback — `on_commit_edit`

```rust
// components.rs:283
on_commit_edit: Option<EventHandler<CommittedEdit<TRow>>>
```

This one **is** an `EventHandler` (unlike `detail_renderer`). Build with
`EventHandler::new(move |edit: CommittedEdit<TRow>| {…})`.

`CommittedEdit<TRow>` fields (`types.rs:90`, `#[non_exhaustive]`,
`TRow: Clone`):
- `row_id: RowId`
- `column_id: ColumnId`
- `value: String` — the **raw string** the user typed
- `prior: PriorEdit<TRow>` — `{ row_id, column_id, prior_row: TRow }` snapshot
  for optimistic rollback (`types.rs:75`).

The host applies the edit: read the current row out of
`handle.signal().read().rows`, mutate the matched field from `edit.value`, then
`handle.update_row(edit.row_id, new_row)` (`hooks.rs:187`). chorale fires the
callback **on blur and on Enter** and handles Esc=cancel / Tab=next-editable
internally (`editor_td`, `components.rs:2654-2752`).

Optional pre-commit validation: `validate_edit: ValidateEditFn`
(`components.rs:280`), build with `ValidateEditFn::new(|EditValidation| ->
Result<(),String>)`. `Err(msg)` shows inline and keeps the editor open.

> Canonical usage: `examples/qa-harness/src/main.rs` — `commit_handler`
> (L447-465: matches `edit.column_id`, mutates, `update_row`) and the
> `name` column conditionally getting `.editor(EditorKind::Text)` (L243-244).

### ⚠ GOTCHA — there is NO native `<select>` / dropdown editor in v0.2.0

The budget spec wants the **category** editor to be a select/dropdown. chorale
v0.2.0 does **not** provide one. `editor_td` (`components.rs:2591`) renders an
`<input>` for every `EditorKind`, and its `input_type` match falls through to
`"text"` for both `Text` and `Custom` (`components.rs:2620-2625`). The
`cell_renderers` prop is consulted **only in read mode** (`data_td`,
`components.rs:2452`), never for the active editor cell — so you cannot inject a
custom `<select>` through `cell_renderers` either.

**Implications for the build agent — pick one:**
1. Ship the category editor as a **text input** (`EditorKind::Text`) for the
   first cut, with `validate_edit` enforcing membership in the category list
   (`Err("unknown category")` otherwise). Lowest effort, compiles today.
2. Build the dropdown **outside** chorale's edit path: render the category cell
   read-only and trigger your own `<select>` overlay on click, then call
   `handle.update_row(...)` directly (skip `on_commit_edit` entirely for that
   column).
3. File a chorale feature request for a `EditorKind::Select { options }` (or a
   custom-editor renderer hook) and gate the dropdown on it.

The compile-verified snippet below uses option (1) (`EditorKind::Text` on
`category`) because that is what the real API supports.

**Budget mapping:** transaction child table — `category` and `comment` columns
get `.editor(EditorKind::Text)`; `amount`/`date`/etc. omit `.editor()` and stay
read-only; `on_commit_edit` matches on `edit.column_id` and `update_row`s.

---

## 4. ColumnDef recipe card

| Want | Recipe |
|------|--------|
| Read-only column | `ColumnDef::new(id, header, accessor)` — that's it. No `.editor()`. |
| Editable (text) column | `…new(...).editor(EditorKind::Text)` |
| Editable (number) column | `…new(...).editor(EditorKind::Number { min, max, step })` |
| Aggregated column (group subtotal) | `…new(...).aggregator(AggregatorKind::Sum)` |
| Currency display | `…new(...).render_kind(RenderKind::Currency(CurrencyCode::USD)).alignment(Alignment::Right)` |
| Date display | `…new(...).render_kind(RenderKind::Date)` |
| Sortable header | `…new(...).sortable()` |
| Frozen column | `…new(...).frozen(FrozenSide::Left)` |

`ColumnDef::new(id, header, accessor)` (`column.rs:295`) — `accessor` is
`impl Fn(&TRow) -> CellValue + Send + Sync + 'static`; all builder methods are
`#[must_use]` and chain. The struct is `#[non_exhaustive]` — always build via
`new(...)` + builders, never a struct literal.

---

## 5. Other gotchas / breaking changes

- **`toggle_sort` now requires a `SortAction`** (BREAKING, CHANGELOG ⚠).
  `handle.toggle_sort(col, SortAction::Replace)` for the old single-column
  behavior; `SortAction::Append` is Shift-click multi-sort
  (`hooks.rs:56`, `SortAction` `types.rs:259`). Not used in the snippet but you
  will hit it the moment you wire a sortable header callback.
- **`detail_renderer` is `Callback`, `on_commit_edit` is `EventHandler`.** Two
  different Dioxus closure wrappers on the same component — easy to swap by
  mistake. CHANGELOG's "`EventHandler<TRow, Element>` prop" line for
  `detail_renderer` is stale; trust `components.rs:294`.
- **RowId identity must be stable.** Generate `RowId`s once when you build the
  row set (server-fetch boundary), not per render. Expansion (`expanded_rows`),
  selection, and editing all key off `RowId`.
- **No `Decimal` CellValue.** Money → `CellValue::Float` at the accessor only;
  keep `rust_decimal::Decimal` as source of truth (BUDGET-MONEY-1).
- **`xlsx` is feature-gated.** `xlsx_export: bool` and `ExportXlsxButton` compile
  without the feature but render nothing unless `chorale-dioxus/xlsx` (→
  `chorale-core/xlsx`, pulls `rust_xlsxwriter`) is enabled. Not needed for the
  ledger; leave it off.
- **Variable-row-height + detail panels.** Detail panels are inherently
  variable-height; chorale turns on row measurement automatically when
  `detail_renderer` is set (`components.rs:333-340`). Don't fight it by forcing
  a fixed `row_height` for the parent.
- **`group_header_class` default** is `"chorale-group-header"` — style that class
  in the app stylesheet for the subtotal rows to look distinct.

---

## 6. COMPILE-VERIFIED SNIPPET

> **Compile-verified against chorale `main` @ `2862159` via:**
> ```
> cargo check -p budget-ui --no-default-features --features web --target wasm32-unknown-unknown
> ```
> Result: **clean** (only `dead_code` warnings, because the demo component is not
> mounted in the router — the API surface itself compiles). Built as a throwaway
> `crates/budget-ui/src/_chorale_recon.rs` module, verified, then removed; the
> code lives here only.
>
> It exercises **all three features together**: a parent day-ledger `<Table>`
> whose `detail_renderer` mounts a child transaction `<Table>`; the child grouped
> by `category` with a `Sum` aggregator on `amount`; and `category` + `comment`
> as `EditorKind::Text` editor columns wired to `on_commit_edit`, with
> `amount`/`date` read-only.

```rust
use chorale_core::{
    AggregatorKind, Alignment, CellValue, ColumnDef, ColumnId, CommittedEdit, CurrencyCode,
    EditorKind, RenderKind, RowId, TableState,
};
use chorale_dioxus::{use_table, Table, UseTableHandle};
use dioxus::prelude::*;

// ── Fake in-memory data ──────────────────────────────────────────────────────

#[derive(Clone, PartialEq)]
struct Day {
    date: chrono::NaiveDate,
    label: String,
    total: f64,
}

#[derive(Clone, PartialEq)]
struct Txn {
    category: String,
    comment: String,
    amount: f64,
}

fn day_rows() -> Vec<(RowId, Day)> {
    vec![
        (
            RowId::new(),
            Day {
                date: chrono::NaiveDate::from_ymd_opt(2026, 6, 1).unwrap(),
                label: "Mon Jun 1".into(),
                total: -42.50,
            },
        ),
        (
            RowId::new(),
            Day {
                date: chrono::NaiveDate::from_ymd_opt(2026, 6, 2).unwrap(),
                label: "Tue Jun 2".into(),
                total: -118.00,
            },
        ),
    ]
}

fn txn_rows() -> Vec<(RowId, Txn)> {
    vec![
        (
            RowId::new(),
            Txn {
                category: "Groceries".into(),
                comment: "Trader Joe's".into(),
                amount: -22.50,
            },
        ),
        (
            RowId::new(),
            Txn {
                category: "Groceries".into(),
                comment: "Bodega".into(),
                amount: -8.00,
            },
        ),
        (
            RowId::new(),
            Txn {
                category: "Transport".into(),
                comment: "Subway".into(),
                amount: -12.00,
            },
        ),
    ]
}

// ── Parent (day-ledger) columns ──────────────────────────────────────────────

fn day_columns() -> Vec<ColumnDef<Day>> {
    vec![
        ColumnDef::new(ColumnId("date"), "Date", |d: &Day| CellValue::Date(d.date))
            .render_kind(RenderKind::Date)
            .initial_width(120.0),
        ColumnDef::new(ColumnId("label"), "Day", |d: &Day| {
            CellValue::Text(d.label.clone())
        })
        .initial_width(160.0),
        ColumnDef::new(ColumnId("total"), "Net", |d: &Day| CellValue::Float(d.total))
            .alignment(Alignment::Right)
            .render_kind(RenderKind::Currency(CurrencyCode::USD)),
    ]
}

// ── Child (transaction) columns — category + comment editable, rest read-only ─

fn txn_columns() -> Vec<ColumnDef<Txn>> {
    vec![
        // EDITOR column (category). NOTE: chorale v0.2.0 has no native <select>
        // editor — EditorKind::Custom still renders a text <input> in editor_td.
        // A real dropdown must be built host-side (see cheat-sheet §3 gotcha).
        ColumnDef::new(ColumnId("category"), "Category", |t: &Txn| {
            CellValue::Text(t.category.clone())
        })
        .editor(EditorKind::Text)
        .initial_width(140.0),
        // EDITOR column (comment) — free text.
        ColumnDef::new(ColumnId("comment"), "Comment", |t: &Txn| {
            CellValue::Text(t.comment.clone())
        })
        .editor(EditorKind::Text)
        .initial_width(220.0),
        // READ-ONLY + AGGREGATED column (amount). No .editor() => read-only.
        // .aggregator(Sum) => per-category subtotal in the group header.
        ColumnDef::new(ColumnId("amount"), "Amount", |t: &Txn| {
            CellValue::Float(t.amount)
        })
        .alignment(Alignment::Right)
        .render_kind(RenderKind::Currency(CurrencyCode::USD))
        .aggregator(AggregatorKind::Sum),
    ]
}

// ── Child table: grouped by category, with category+comment editors ──────────

#[component]
fn TxnChildTable() -> Element {
    let rows = txn_rows();
    let n = rows.len();
    let table: UseTableHandle<Txn> = use_table(move || {
        let mut s = TableState::new(rows.clone(), txn_columns());
        s.page_size = n.max(1);
        // Turn grouping ON: group by the category column.
        s.grouping = vec![ColumnId("category")];
        s
    });

    let on_commit: EventHandler<CommittedEdit<Txn>> =
        EventHandler::new(move |edit: CommittedEdit<Txn>| {
            let current = table
                .signal()
                .read()
                .rows
                .iter()
                .find(|(id, _)| *id == edit.row_id)
                .map(|(_, r)| r.clone());
            if let Some(mut row) = current {
                match edit.column_id {
                    ColumnId("category") => row.category = edit.value.clone(),
                    ColumnId("comment") => row.comment = edit.value.clone(),
                    _ => {}
                }
                table.update_row(edit.row_id, row);
            }
        });

    rsx! {
        Table {
            handle: table,
            inline: true,
            on_commit_edit: on_commit,
        }
    }
}

// ── Parent table: day rows, each expandable to the child txn table ───────────

#[component]
pub fn ChoraleReconDemo() -> Element {
    let table: UseTableHandle<Day> = use_table(move || {
        let mut s = TableState::new(day_rows(), day_columns());
        s.page_size = 31;
        s
    });

    rsx! {
        Table {
            handle: table,
            sort_enabled: true,
            // Master/detail: chevron column + full-width detail <tr> mounting
            // a child <Table>. Callback receives the parent Day row by value.
            detail_renderer: Callback::new(move |_day: Day| {
                rsx! { TxnChildTable {} }
            }),
        }
    }
}
```

---

## 7. Feature → budget-tracker mapping (summary)

| chorale feature | budget-tracker use | key API |
|---|---|---|
| Master/detail (Item 12) | Day-ledger parent table; each day expands to its transactions | `detail_renderer: Callback<Day, Element>`, child `Table { inline: true }` |
| Grouping + aggregation (Item 8) | Child transaction table grouped by category, per-category subtotal | `s.grouping = vec![ColumnId("category")]` / `handle.set_grouping(...)`; `.aggregator(AggregatorKind::Sum)` on `amount` |
| In-cell editing (Item 7) | `category` + `comment` editable; `amount`/`date` read-only | `.editor(EditorKind::Text)` on the two columns; `on_commit_edit: EventHandler<CommittedEdit<Txn>>` + `handle.update_row` |

**Open decision for the build agent:** the category editor wants a dropdown;
chorale v0.2.0 has no native `<select>` editor (§3 gotcha). Default to
`EditorKind::Text` + `validate_edit` membership check, or build a host-side
dropdown overlay calling `update_row` directly.
