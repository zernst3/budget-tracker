# AI Portfolio Insights — End-to-End Design (Build-Ready)

Status: LOCKED. Every signature, field, constant, and table column below is final and one-way-door. This document is the single source of truth handed to the build agent. It is organized by build phase (§Phase 0 through §Phase 7); each phase lists the exact files to touch, the types and signatures to write, and the test list to satisfy.

Provenance: mirrors `plaid_api.rs` (port + DTO shape), `auth.rs::SecretVault` (vault port + error category), `plaid/wire.rs` (wire isolation), `MockPlaidApi` (fixture-driven mock), `money.rs` (exact arithmetic, `from_minor`, `round_to_cents`), `config.rs::DeficitFinancingConfig` (pinned-constant style), `m0004` (migration hygiene).

---

## 0. Cross-crate consistency verification (performed before this doc was finalized)

The five invariants the judgment tier was required to verify, and their resolution:

| # | Invariant | Result |
|---|---|---|
| 1 | `InvestmentAdvisor` port surface exactly matches what `GeneratePortfolioReview` consumes | **PASS** — see §0.1 |
| 2 | `reconcile()` is an exhaustive match over every `ClaimSubject` variant | **PASS** — see §0.2 |
| 3 | No domain type carries an http/wire/Gemini dependency | **PASS** — see §0.3 |
| 4 | `review_runs` columns match exactly what app-services writes | **PASS with one mismatch resolved** — see §0.4 |
| 5 | The wire structs map 1:1 to `Claim`/`ClaimSubject` | **PASS with one naming mismatch resolved** — see §0.5 |

### 0.1 Advisor port ↔ use-case (PASS)

Port surface (§Phase-1, locked):

```rust
async fn recommend(&self, snapshot: &PortfolioSnapshot) -> Result<AdvisorOutput, AdvisorError>;
fn model_id(&self) -> &str;
```

`AdvisorOutput { recommendations, raw_output, prompt_hash, prompt_tokens, completion_tokens }`.

The use-case (§Phase-5 flow) consumes exactly this surface:
- `advisor.recommend(&snapshot)` — the only call.
- `Err(AdvisorError::Parse(raw))` → persist `MalformedOutput` with the raw string. The raw string is also carried inside `AdvisorOutput.raw_output` on the success path; on the `Parse` error path it travels in the error payload. Both paths feed `review_runs.raw_output`. Consistent.
- `advisor.model_id()` → `review_runs.model_id`.
- `AdvisorOutput.{prompt_hash, prompt_tokens, completion_tokens}` → the matching `review_runs` columns.
- `AdvisorOutput.recommendations` → reconciled, then `outcomes`.

Every field the use-case writes to the audit row is sourced either from `AdvisorOutput`, from `model_id()`, or computed locally (`latency_ms`, `occurred_at`, `snapshot`). **No field is consumed that the port does not provide. No field the port provides is dropped except by deliberate design (`AdvisorOutput.raw_output` is persisted; nothing else is unused).**

### 0.2 reconcile() exhaustiveness (PASS)

`ClaimSubject` has exactly three variants: `Position { ticker }`, `Buffer`, `NetWorth`. `reconcile_claim` matches all three with no wildcard arm. Adding a fourth variant is a compile error — this is the `BUDGET-AI-1` enforcement mechanism and must never be defeated with a `_ =>` arm.

### 0.3 Domain dependency isolation (PASS)

`budget-domain/src/portfolio.rs` module header forbids `reqwest`, Gemini types, and `serde_json::Value`. `serde` derives are permitted (already a domain dep). All Gemini wire structs live in `budget-infrastructure/src/advisor/wire.rs`. The domain `AdvisorError` carries only `String` payloads — never an `http` status, never a Gemini error object, never the API key.

### 0.4 review_runs columns ↔ app-services writes (RESOLVED MISMATCH)

The migration DDL, the SeaORM entity `Model`, and the use-case persist step were cross-checked column by column. **One mismatch was found and resolved:**

- **`outcomes` JSONB shape.** The domain `ReviewRun.outcomes` is typed `Vec<(usize, ValidationOutcome)>` (per-rec index → outcome). The migration comment in one module-design draft said "per-recommendation `ValidationOutcome` array" (i.e. a bare `Vec<ValidationOutcome>` positional array), while another said "per-recommendation `(index, ValidationOutcome)` pairs." These serialize to **different JSON** (`[[0,{...}],[1,{...}]]` vs `[{...},{...}]`).
  **Resolution (LOCKED):** `outcomes` is the index-paired form `Vec<(usize, ValidationOutcome)>`, serialized as a JSON array of two-element arrays. The mapper (Phase 6) MUST serialize/deserialize this exact shape. The stale-quote terminal-state test (§Phase-5 tests) reads `outcomes` by index, which requires the paired form. The migration column stays `outcomes JSONB NOT NULL`; only the documented serde shape is pinned here.

All other columns match exactly:

| review_runs column | written by use-case from | entity Model field |
|---|---|---|
| `id` | `ReviewRunId::generate()` | `id: Uuid` |
| `user_id` | use-case arg | `user_id: Uuid` |
| `model_id` | `advisor.model_id()` | `model_id: String` |
| `prompt_hash` | `AdvisorOutput.prompt_hash` | `prompt_hash: String` |
| `raw_output` | `AdvisorOutput.raw_output` / `Parse(raw)` | `raw_output: String` |
| `snapshot` | assembled `PortfolioSnapshot` (JSONB) | `snapshot: Json` |
| `outcomes` | `Vec<(usize, ValidationOutcome)>` (JSONB) | `outcomes: Json` |
| `terminal_state` | classified `ReviewTerminalState` | `terminal_state: ReviewTerminalStateEntity` |
| `prompt_tokens` | `AdvisorOutput.prompt_tokens` | `prompt_tokens: Option<i64>` |
| `completion_tokens` | `AdvisorOutput.completion_tokens` | `completion_tokens: Option<i64>` |
| `latency_ms` | measured around `recommend()` | `latency_ms: i64` |
| `occurred_at` | use-case `now` arg | `occurred_at: DateTimeWithTimeZone` |

Note on the entity enum name: two module designs named the entity enum `ReviewTerminalState` and `ReviewTerminalStateEntity` respectively. **Resolution (LOCKED):** the entity enum is named **`ReviewTerminalStateEntity`** to avoid a name collision with the domain `ReviewTerminalState` when both are imported into the mapper. The mapper converts between `budget_domain::portfolio::ReviewTerminalState` and `budget_entities::review_runs::ReviewTerminalStateEntity`.

### 0.5 Wire structs ↔ Claim/ClaimSubject (RESOLVED NAMING MISMATCH)

`WireClaim { subject: WireClaimSubject, cited_value: String, cited_percentage: Option<String> }` maps 1:1 to `Claim { subject, cited_value: Money, cited_percentage: Option<Decimal> }`. `WireClaimSubject { kind: String, ticker: Option<String> }` maps to `ClaimSubject` via:

- `kind == "position"` + `ticker: Some(t)` → `ClaimSubject::Position { ticker: Ticker::try_new(t)? }`
- `kind == "buffer"` → `ClaimSubject::Buffer`
- `kind == "net_worth"` → `ClaimSubject::NetWorth`
- any other `kind`, or `"position"` without a ticker → `AdvisorError::Parse`

**Resolution (LOCKED):** the wire discriminant string for `NetWorth` is the snake_case `"net_worth"` (not `"networth"` / `"NetWorth"`). The Gemini `responseSchema` enum for the subject `type` field MUST be exactly `["position", "buffer", "net_worth"]`. The fixture JSON and the `responseSchema` share this constant; a drift fails the mock's round-trip test. This is the single place the wire and domain vocabularies meet, so it is pinned here.

---

## Phase 0 — Green-base drift gate (BLOCKING; not part of this feature's diff)

Two drift fixes from `docs/DRIFT_REPORT.md` MUST land before any Portfolio Insights code, because both touch the money-math substrate this feature reconciles against. Cite `PROC-REGRESSION-TEST-1` on each; each ships a failing-before / passing-after test.

- **MUST-FIX #1** — `MonthViewState` zero-income wiring (`server_state.rs:223-226`): thread real income expectation into `month_net_for`. Reconciling "ground truth" on top of an inflated rollover reconciles against a corrupted figure.
- **MUST-FIX #2 / SHOULD-FIX #5** — delete the redundant `month_net` / `MONTH_NET_SQL` path (`transactions.rs:126-131`) rather than patch it. Net worth (§Phase-2) and budget totals must not share a latent mis-filtered aggregate.

Gate: the base app must build and test green with both fixes in before Phase 1 opens.

---

## Phase 1 — Domain types, ports, schema, entities, mappers

### 1.A Files

```
crates/budget-domain/src/portfolio.rs              NEW — types + 4 ports + 2 error enums + AdvisorOutput
crates/budget-domain/src/ids.rs                    ADD PositionId, ReviewRunId (uuid_newtype!)
crates/budget-domain/src/repositories.rs           ADD ReviewRunRepository, PositionRepository, CashBalanceRepository
crates/budget-domain/src/lib.rs                     ADD pub mod portfolio + re-exports
crates/budget-entities/src/positions.rs            NEW — SeaORM entity (reuses accounts::AccountType)
crates/budget-entities/src/cash_balances.rs        NEW — SeaORM entity
crates/budget-entities/src/review_runs.rs          NEW — SeaORM entity + ReviewTerminalStateEntity
crates/budget-entities/src/lib.rs                   ADD 3 module declarations
crates/budget-mappers/src/positions.rs             NEW — Model <-> Position + tests
crates/budget-mappers/src/cash_balances.rs         NEW — Model <-> CashBalance + tests
crates/budget-mappers/src/lib.rs                    ADD 2 module declarations
crates/budget-migration/src/m0007_portfolio_insights.rs  NEW — DDL + structural tests
crates/budget-migration/src/lib.rs                  ADD mod + register in Migrator vec
```

> Entity file location: the two entity-tier designs disagreed on path (`budget-entities/src/positions.rs` vs `budget-entities/src/entities/positions.rs`). **Resolution (LOCKED):** follow the existing `budget-entities` layout convention — match wherever `users.rs` / `transactions.rs` currently live (confirm at build time). All `super::` paths in the entity code assume `users` is a sibling module of `positions`/`cash_balances`/`review_runs`. If the crate uses an `entities/` subdir, place all three there and keep `super::users` correct.

### 1.B `budget-domain/src/ids.rs` additions

```rust
uuid_newtype!(
    /// Identifies a [`crate::portfolio::Position`].
    PositionId
);
uuid_newtype!(
    /// Identifies a [`crate::portfolio::ReviewRun`] (the audit row for one
    /// portfolio-review invocation).
    ReviewRunId
);
```

No new id tests required — the `uuid_newtype!` macro is proven.

### 1.C `budget-domain/src/portfolio.rs`

Single bounded-context module (`RUST-DOMAIN-1`). WASM-clean: no `reqwest`, no `serde_json::Value`, no SeaORM. `serde` derives allowed. All async ports: `#[async_trait]`, `Send + Sync`, object-safe.

#### Ticker (validated newtype)

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Ticker(String);

impl Ticker {
    const MAX_LEN: usize = 10;
    /// Construct a validated ticker (uppercase, 1–10 of `[A-Z.]`).
    /// # Errors
    /// `ValidationError::{Empty,TooLong,Format}`.
    pub fn try_new(raw: &str) -> Result<Self, ValidationError>;
    #[must_use] pub fn as_str(&self) -> &str;
    #[must_use] pub fn into_string(self) -> String;
}
// + impl Display
```

Behavior: `try_new` trims and uppercases before validating, so `"aapl"` → `Ok("AAPL")`. Accepts `"BRK.A"`. Rejects empty, >10 chars, digits, embedded spaces.

#### Price provenance + quote

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriceProvenance {
    Market { source: String },   // e.g. "finnhub"
    Manual,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PriceQuote {
    pub price: Money,
    pub provenance: PriceProvenance,
    pub as_of: DateTime<Utc>,    // UTC-anchored (ARCH-UTC-TIMESTAMPS-1)
}
```

#### Position

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub id: PositionId,
    pub user_id: UserId,
    pub ticker: Ticker,
    pub account_label: String,
    pub account_type: AccountType,   // reuse budget_domain::enums::AccountType
    pub shares: Decimal,             // a COUNT, not Money (BUDGET-MONEY-1)
    pub cost_basis: Option<Money>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
```

> The locked-decisions field list omitted `created_at`/`updated_at` on `Position`; the mappers tier added them (the entity has them, and the mapper round-trips them). **Resolution (LOCKED):** `Position` carries `created_at`/`updated_at: DateTime<Utc>` so the mapper is total against the entity. They are not used by `reconcile` (reconciliation keys off `ticker` + market value only).

#### CashBalance

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CashBalance {
    pub account_label: String,
    pub balance: Money,      // a BALANCE, never a flow (BUDGET-CASH-1)
    pub reserved: bool,      // true => buffer / non-investable reserve
}
```

#### NetWorth

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetWorth {
    pub total_cash: Money,
    pub total_positions: Money,
    pub liabilities: Money,   // v1: always Money::ZERO (flag, don't assume)
    pub total: Money,         // total_cash + total_positions - liabilities
}
```

#### PricedPosition + PortfolioSnapshot

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PricedPosition {
    pub position: Position,
    pub quote: Option<PriceQuote>,       // None => failed/stale and no manual fallback
    pub market_value: Option<Money>,     // shares * price, round_to_cents; None iff quote None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortfolioSnapshot {
    pub user_id: UserId,
    pub positions: Vec<PricedPosition>,
    pub cash_balances: Vec<CashBalance>,
    pub buffer_total: Money,       // sum of reserved balances
    pub net_worth: NetWorth,
    pub total_invested: Money,     // sum of resolved market_values (skips None)
    pub captured_at: DateTime<Utc>,
}
```

#### Recommendation / Claim / ClaimSubject

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recommendation {
    pub title: String,
    pub rationale: String,
    pub claims: Vec<Claim>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub subject: ClaimSubject,
    pub cited_value: Money,
    /// Optional cited "% of portfolio" for a Position subject, as a RATIO
    /// (e.g. 0.4 for 40%). Buffer/NetWorth MUST carry None (enforced in reconcile).
    pub cited_percentage: Option<Decimal>,
}

/// CLOSED enum — adding a variant forces a new reconcile arm (BUDGET-AI-1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimSubject {
    Position { ticker: Ticker },
    Buffer,
    NetWorth,
}
```

> `cited_percentage` unit (LOCKED): it is a **ratio** in `[0,1]`, not a 0–100 percentage. `reconcile` compares it against `market_value / total_invested` (also a ratio) at `PERCENT_PRECISION_DP`. The UI formatter multiplies by 100 for display. This is the single unit convention; the `responseSchema` description MUST instruct the model to emit the ratio.

#### Outcomes

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ValidationOutcome {
    Verified,
    Unverified(UnverifiedReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnverifiedReason {
    UnknownTicker(String),
    ValueMismatch { cited: Money, ground_truth: Money },
    MissingMarketData(String),
    PercentageMismatch { cited: Decimal, ground_truth: Decimal },
    MalformedClaim(String),
}
```

A recommendation's displayed outcome is the **worst** across its claims (any `Unverified` makes the whole recommendation `Unverified`).

#### ReviewRun + ReviewTerminalState

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewRun {
    pub id: ReviewRunId,
    pub user_id: UserId,
    pub model_id: String,
    pub prompt_hash: String,
    pub raw_output: String,
    pub snapshot: PortfolioSnapshot,
    pub outcomes: Vec<(usize, ValidationOutcome)>,  // per-rec index -> outcome (LOCKED shape, §0.4)
    pub terminal_state: ReviewTerminalState,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    pub latency_ms: i64,
    pub occurred_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewTerminalState {
    Completed,             // >=1 verifiable rec. SUCCESS.
    NoVerifiableInsights,  // valid JSON, zero recs OR zero verifiable. SUCCESS.
    EmptyPortfolio,        // short-circuit before model call. SUCCESS.
    MalformedOutput,       // parse failure. FAILURE-of-review (run still persisted).
}
```

A stale/failed quote is NOT a terminal state — it degrades the position to `quote: None`, making any citing claim `Unverified(MissingMarketData)` by construction.

> `ReviewRun` does NOT carry `Vec<Recommendation>`. The Phase-6 mapper (`review_run_to_dto`) needs the recommendation titles/rationales to render cards. **Resolution (LOCKED, Phase-6 decision pinned now):** add `pub recommendations: Vec<Recommendation>` to `ReviewRun`, persisted inside the `snapshot` is NOT correct (snapshot is ground truth, not model output). Instead the recommendations are reconstructable from `raw_output` is fragile. **Final:** `ReviewRun` gains `pub recommendations: Vec<Recommendation>`, and the migration adds a `recommendations JSONB NOT NULL` column to `review_runs`. See §0.4-addendum below. This is a one-line schema addition done now in m0007, not deferred, so the audit row is self-contained.

##### §0.4 addendum — recommendations column (LOCKED)

`review_runs` gains one column beyond the table in Phase 1.E:

| `recommendations` | `JSONB NOT NULL` | the model's parsed `Vec<Recommendation>` (empty array for EmptyPortfolio/NoVerifiableInsights-with-zero-recs) |

The use-case writes it from `AdvisorOutput.recommendations`. The mapper deserializes it for `review_run_to_dto`. `outcomes[i]` indexes into `recommendations[i]`.

#### Ports

```rust
#[async_trait]
pub trait InvestmentAdvisor: Send + Sync {
    /// # Errors
    /// `AdvisorError::{Api,RateLimited,Unavailable,Parse,SecretVault}`.
    async fn recommend(&self, snapshot: &PortfolioSnapshot) -> Result<AdvisorOutput, AdvisorError>;
    fn model_id(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdvisorOutput {
    pub recommendations: Vec<Recommendation>,
    pub raw_output: String,
    pub prompt_hash: String,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
}

#[async_trait]
pub trait MarketDataProvider: Send + Sync {
    /// Ok(None) => no quote for that ticker (caller falls back / degrades).
    /// # Errors
    /// `MarketDataError::{Api,RateLimited,SecretVault}`.
    async fn quote(&self, ticker: &Ticker) -> Result<Option<PriceQuote>, MarketDataError>;
}

#[async_trait]
pub trait PositionSource: Send + Sync {
    /// # Errors RepositoryError
    async fn positions_for_user(&self, user_id: UserId) -> Result<Vec<Position>, RepositoryError>;
}

#[async_trait]
pub trait CashBalanceSource: Send + Sync {
    /// # Errors RepositoryError
    async fn balances_for_user(&self, user_id: UserId) -> Result<Vec<CashBalance>, RepositoryError>;
}
```

#### Port error enums

```rust
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AdvisorError {
    #[error("advisor api failure: {0}")] Api(String),
    #[error("advisor rate limited: {0}")] RateLimited(String),
    #[error("advisor unavailable: {0}")] Unavailable(String),
    #[error("advisor output parse failure: {0}")] Parse(String),
    #[error("secret vault failure: {0}")] SecretVault(String),
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarketDataError {
    #[error("market data api failure: {0}")] Api(String),
    #[error("market data rate limited: {0}")] RateLimited(String),
    #[error("secret vault failure: {0}")] SecretVault(String),
}
```

`AdvisorError`/`MarketDataError` carry NO secret material. `PositionSource`/`CashBalanceSource` reuse the existing `RepositoryError`.

### 1.D `budget-domain/src/repositories.rs` additions

```rust
#[async_trait]
pub trait ReviewRunRepository: Send + Sync {
    async fn insert(&self, run: &ReviewRun, uow: &mut dyn UnitOfWork) -> Result<(), RepositoryError>;
    async fn list_for_user(&self, user_id: UserId) -> Result<Vec<ReviewRun>, RepositoryError>;
}

#[async_trait]
pub trait PositionRepository: PositionSource {
    async fn insert(&self, position: &Position) -> Result<(), RepositoryError>;
    async fn update(&self, position: &Position) -> Result<(), RepositoryError>;
    async fn delete(&self, user_id: UserId, id: PositionId) -> Result<(), RepositoryError>;
}

#[async_trait]
pub trait CashBalanceRepository: CashBalanceSource {
    async fn upsert(&self, balance: &CashBalance) -> Result<(), RepositoryError>;
}
```

### 1.E Entities

**`positions.rs`** — `DeriveEntityModel`, `table_name = "positions"`, no serde on `Model`. Reuses `super::accounts::AccountType` (do NOT redeclare the `account_type` pg-enum). Fields: `id: Uuid` (PK, no auto-increment), `user_id`, `ticker: String`, `account_label: String`, `account_type: AccountType`, `shares: Decimal`, `cost_basis: Option<Decimal>`, `created_at`/`updated_at: DateTimeWithTimeZone`. `Relation::User` belongs_to `users` ON DELETE Cascade. Empty `ActiveModelBehavior`. Module doc cites `m0007` and the `(user_id, ticker, account_label)` unique.

> The infrastructure-tier draft showed a `PositionAccountType` inline enum — that is WRONG and explicitly corrected: use `super::accounts::AccountType`.

**`cash_balances.rs`** — `Model { id, user_id, account_label: String, balance: Decimal, reserved: bool, created_at, updated_at }`. `Relation::User` Cascade. Empty `ActiveModelBehavior`. Doc cites `m0007` + `(user_id, account_label)` unique + `BUDGET-CASH-1`.

**`review_runs.rs`** — declares `ReviewTerminalStateEntity` (`DeriveActiveEnum`, `enum_name = "review_terminal_state"`, string values `completed`/`no_verifiable_insights`/`empty_portfolio`/`malformed_output`). `Model { id, user_id, model_id: String, prompt_hash: String, raw_output: String, snapshot: Json, outcomes: Json, recommendations: Json, terminal_state: ReviewTerminalStateEntity, prompt_tokens: Option<i64>, completion_tokens: Option<i64>, latency_ms: i64, occurred_at: DateTimeWithTimeZone }`. `Relation::User` Cascade; NO reverse `has_many` (audit-log semantics). NO `updated_at` (append-only). Empty `ActiveModelBehavior`. Doc cites `m0007` + `SQL-AUDIT-COLUMNS-1`.

### 1.F Migration `m0007_portfolio_insights.rs`

Raw guarded DDL mirroring `m0004`. Idempotent `CREATE TYPE` in a `duplicate_object` catch; `CREATE TABLE IF NOT EXISTS`; named FK constraints guarded; `CREATE INDEX IF NOT EXISTS`. No user data seeded. Expand-only.

`up` execution order: (1) `review_terminal_state` enum, (2) `positions` + FK + `ix_positions_user_id` + `uq_positions_user_ticker_account`, (3) `cash_balances` + FK + `ix_cash_balances_user_id` + `uq_cash_balances_user_account`, (4) `review_runs` + FK + `ix_review_runs_user_id` + `ix_review_runs_user_occurred (user_id, occurred_at DESC)`.

`positions` columns: `id UUID PK`, `user_id UUID NOT NULL`, `ticker TEXT NOT NULL`, `account_label TEXT NOT NULL`, `account_type account_type NOT NULL`, `shares NUMERIC NOT NULL`, `cost_basis NUMERIC` (nullable), `created_at TIMESTAMPTZ NOT NULL`, `updated_at TIMESTAMPTZ NOT NULL`.

`cash_balances` columns: `id UUID PK`, `user_id UUID NOT NULL`, `account_label TEXT NOT NULL`, `balance NUMERIC NOT NULL`, `reserved BOOLEAN NOT NULL DEFAULT false`, `created_at`/`updated_at TIMESTAMPTZ NOT NULL`.

`review_runs` columns (system-log, `SQL-AUDIT-COLUMNS-1`, no `created_by`/`modified_by`, no `updated_at`): `id UUID PK`, `user_id UUID NOT NULL`, `model_id TEXT NOT NULL`, `prompt_hash TEXT NOT NULL`, `raw_output TEXT NOT NULL`, `snapshot JSONB NOT NULL`, `outcomes JSONB NOT NULL`, `recommendations JSONB NOT NULL` (§0.4-addendum), `terminal_state review_terminal_state NOT NULL`, `prompt_tokens BIGINT`, `completion_tokens BIGINT`, `latency_ms BIGINT NOT NULL`, `occurred_at TIMESTAMPTZ NOT NULL`.

`down`: drop in reverse dependency order — indexes → constraints → tables (review_runs, cash_balances, positions) → `DROP TYPE IF EXISTS review_terminal_state` last.

Register in `lib.rs`: `mod m0007_portfolio_insights;` + `Box::new(m0007_portfolio_insights::Migration)` appended to the `Migrator` vec.

### 1.G Mappers

`positions.rs`: `model_to_domain(Model) -> Result<Position, MapperError>` (ticker via `Ticker::try_new`, fails to `MapperError::InvalidStoredValue { field: "ticker", .. }`; `cost_basis: Option<Decimal>` → `Option<Money>`; account_type 1:1; timestamps → UTC). `domain_to_active_model(&Position) -> ActiveModel`. Local `account_type_to_domain`/`account_type_to_entity` helpers.

`cash_balances.rs`: `model_to_domain(Model) -> Result<CashBalance, MapperError>` (total, infallible Result for uniform signature). `to_active_model(id, user_id, &CashBalance, now) -> ActiveModel`.

> review_runs mapper is deferred to Phase 6 (it depends on the JSONB serde round-trip of `PortfolioSnapshot`, `outcomes`, and `recommendations`).

### 1.H Phase 1 tests

- **Domain `Ticker` (in `portfolio.rs`):** normalises lowercase→upper; accepts `BRK.A`; accepts single letter; rejects empty, >10 chars, digits, embedded space; round-trips through Display/`into_string`.
- **Mapper `positions.rs`:** round-trips an investment position; rejects an invalid stored ticker → `InvalidStoredValue`; null `cost_basis` → `None`.
- **Mapper `cash_balances.rs`:** reserved balance round-trips; non-reserved maps correctly.
- **Migration `m0007` (DB-free structural, mirror `m0004`):** enum created + guarded with all 4 variants; each of the 3 tables `CREATE TABLE IF NOT EXISTS`; `positions` columns + nullable `cost_basis` + composite unique + FK index; `cash_balances` `reserved BOOLEAN NOT NULL DEFAULT false` + FK index + composite unique; `review_runs` all columns (incl. `recommendations JSONB NOT NULL`) + nullable token columns + NO `updated_at` + FK index + `(user_id, occurred_at DESC)` history index; `down` drops in dependency order (review_runs before enum), all tables + indexes `IF EXISTS`.

### 1.H.1 HARD PAUSE — InvestmentAdvisor surface sign-off

`ORCH-ONE-WAY-DOOR-1`. The port surface is locked as §1.C: `recommend(&PortfolioSnapshot) -> Result<AdvisorOutput, AdvisorError>` + `model_id()`. Returning `AdvisorOutput` (not bare `Vec<Recommendation>`) keeps the audit counters and raw-string on the port boundary — no second round-trip, and the parse-failure path has its raw-string home. This is the sign-off gate; it is now signed off.

---

## Phase 2 — Manual sources + positions UI

### Files
```
crates/budget-infrastructure/src/...  ManualPositionSource (impl PositionRepository), ManualCashBalanceSource (impl CashBalanceRepository)
crates/budget-ui/src/services/portfolio_review.rs   NEW — DTOs + #[server] stubs (this phase wires positions/balances)
crates/budget-ui/src/services/mod.rs                ADD pub mod portfolio_review
crates/budget-ui/src/views/portfolio_review.rs      NEW — read-only positions table + buffer/net-worth display
crates/budget-ui/src/views/mod.rs                   ADD pub mod portfolio_review
crates/budget-ui/src/server_state.rs                ADD PortfolioState (Arc ports + service) + extract()
crates/budget-mappers/src/portfolio.rs              NEW — DTO mappers (position/cash; snapshot+review in Phase 3/6)
crates/budget-mappers/src/lib.rs                    ADD pub mod portfolio
```

### Server functions (gate FIRST — `BUDGET-AUTH-GATE-1`; DTOs only — `ARCH-API-DTOS-1`)

All eight live in `services/portfolio_review.rs`. Phase 2 wires `list_positions`, `add_position`, `edit_position`, `delete_position`, `list_cash_balances`, `upsert_cash_balance`. `portfolio_snapshot` is Phase 3, `run_review` is Phase 6 (stubbed `todo!`/501 until then).

```rust
#[server] pub async fn list_positions() -> Result<Vec<PositionDto>, ServerFnError>;
#[server] pub async fn add_position(input: AddPositionDto) -> Result<PositionDto, ServerFnError>;
#[server] pub async fn edit_position(id: Uuid, input: AddPositionDto) -> Result<PositionDto, ServerFnError>;
#[server] pub async fn delete_position(id: Uuid) -> Result<(), ServerFnError>;
#[server] pub async fn list_cash_balances() -> Result<Vec<CashBalanceDto>, ServerFnError>;
#[server] pub async fn upsert_cash_balance(input: CashBalanceDto) -> Result<CashBalanceDto, ServerFnError>;
#[server] pub async fn portfolio_snapshot() -> Result<PortfolioSnapshotDto, ServerFnError>;  // Phase 3
#[server] pub async fn run_review() -> Result<ReviewResultDto, ServerFnError>;               // Phase 6
```

Each body's first line is `let user = require_authed_user().await?;` then `let state = PortfolioState::extract().await?;`.

### DTOs (serde, WASM-clean, `Money` rendered as String)

```rust
PositionDto { id: Uuid, ticker, account_label, account_type: String, shares: String, cost_basis: Option<String> }
AddPositionDto { ticker, account_label, account_type: String, shares: String, cost_basis: Option<String> }
CashBalanceDto { id: Option<Uuid>, account_label, balance: String, reserved: bool }
PricedPositionDto { ticker, account_label, account_type: String, shares: String,
                    price: Option<String>, provenance: Option<String>, as_of: Option<String>,
                    market_value: Option<String>, pct_of_portfolio: Option<String>, is_stale: bool }
NetWorthDto { total_cash, total_positions, liabilities, total }   // all String
PortfolioSnapshotDto { positions: Vec<PricedPositionDto>, cash_balances: Vec<CashBalanceDto>,
                       buffer_total: String, net_worth: NetWorthDto, total_invested: String, captured_at: String }
ClaimDto { subject: String, cited_value: String, cited_percentage: Option<String>, badge: ValidationBadgeDto }
#[serde(tag="kind")] enum ValidationBadgeDto { Verified, Unverified { reason: String } }
RecommendationDto { title, rationale, badge: ValidationBadgeDto, claims: Vec<ClaimDto>, tax_note: Option<String> }
enum ReviewTerminalStateDto { Completed, NoVerifiableInsights, EmptyPortfolio, MalformedOutput }
ReviewResultDto { run_id: Uuid, terminal_state: ReviewTerminalStateDto,
                  recommendations: Vec<RecommendationDto>, disclaimer: &'static str }

pub const PORTFOLIO_REVIEW_DISCLAIMER: &str = "...not financial advice...";  // N3, on every result
```

> `ClaimDto` subject rendering: the locked decisions and one module draft used a single `subject: String` human string; another used `subject_type: String` + `ticker: Option<String>`. **Resolution (LOCKED):** `ClaimDto.subject` is a single pre-rendered human string (`"AAPL market value"`, `"Reserved cash buffer"`, `"Total net worth"`) produced by the mapper. Raw subject discriminants and raw `UnverifiedReason` codes NEVER cross to the client (`RUST-DIOXUS-10`).

### `PortfolioState` (server_state.rs)

```rust
#[derive(Clone)]
pub struct PortfolioState {
    pub position_source: Arc<dyn PositionRepository>,
    pub balance_source: Arc<dyn CashBalanceRepository>,
    pub market: Arc<dyn MarketDataProvider>,
    pub review_service: Arc<GeneratePortfolioReview>,
}
impl PortfolioState { pub async fn extract() -> Result<Self, ServerFnError>; }
```

### Phase 2 tests
- Manual source round-trip insert/list/update/delete (positions) and upsert/list (balances) — against a test DB or in-memory fake per crate convention.
- Mapper `position_to_dto` / `add_position_dto_to_domain` shares-string parse and account_type string round-trip.

---

## Phase 3 — MarketDataProvider (manual fallback FIRST, then one real adapter)

### Files
```
crates/budget-infrastructure/src/market_data/mod.rs   NEW
crates/budget-infrastructure/src/market_data/mock.rs  NEW — MockMarketDataProvider
crates/budget-infrastructure/src/market_data/<provider>.rs  NEW — real adapter (provider TBD, see Open Items)
crates/budget-infrastructure/src/lib.rs               ADD pub mod market_data
```

Manual-price fallback is wired before any real HTTP adapter. The use-case fans out `market.quote(ticker)` concurrently via `try_join_all` (`ARCH-PARALLEL-INDEPENDENT-1`); a `None`/failed quote falls back to a manual price if the position has one, else degrades to `quote: None`.

`portfolio_snapshot` server fn becomes live this phase: load positions+balances, fetch quotes, assemble + return `PortfolioSnapshotDto`.

### Phase 3 tests
- `MockMarketDataProvider` returns configured quotes/`None`/errors per ticker.
- Snapshot assembly: `total_invested` skips `None` market values; `buffer_total` sums only reserved; `net_worth.total = total_cash + total_positions - 0`; `pct_of_portfolio` rendered to 1 dp; `is_stale` flag set when quote absent/old.

---

## Phase 4 — InvestmentAdvisor port + MockInvestmentAdvisor + fixtures

### Files
```
crates/budget-infrastructure/src/advisor/mod.rs    NEW
crates/budget-infrastructure/src/advisor/wire.rs   NEW — Gemini wire DTOs + parse_advisor_response
crates/budget-infrastructure/src/advisor/mock.rs   NEW — MockInvestmentAdvisor + MockMode
crates/budget-infrastructure/src/advisor/fixtures/gemini_verified.json        NEW
crates/budget-infrastructure/src/advisor/fixtures/gemini_hallucinated.json    NEW (required)
crates/budget-infrastructure/src/advisor/fixtures/gemini_empty_recs.json      NEW
crates/budget-infrastructure/src/lib.rs            ADD pub mod advisor
```

### wire.rs (the wire↔domain boundary — §0.5)

```rust
pub(crate) struct GeminiResponse { candidates: Vec<WireCandidate> }
pub(crate) struct WireCandidate { content: WireContent, finish_reason: String, usage_metadata: Option<WireUsageMetadata> }
pub(crate) struct WireContent { parts: Vec<WirePart> }
pub(crate) struct WirePart { text: String }
pub(crate) struct WireUsageMetadata { prompt_token_count: Option<i64>, candidates_token_count: Option<i64> }
pub(crate) struct WireRecommendations { recommendations: Vec<WireRecommendation> }
pub(crate) struct WireRecommendation { title: String, rationale: String, claims: Vec<WireClaim> }
pub(crate) struct WireClaim { subject: WireClaimSubject, cited_value: String, cited_percentage: Option<String> }
pub(crate) struct WireClaimSubject { #[serde(rename="type")] kind: String, ticker: Option<String> }

/// # Errors AdvisorError::Parse on any decode/validation failure.
pub(crate) fn parse_advisor_response(wire: GeminiResponse) -> Result<AdvisorOutput, AdvisorError>;
```

`parse_advisor_response`: take first candidate; extract `usage`; extract `parts[0].text` as the structured JSON string (JSON-in-text variant — confirm at Phase 6, see Open Items); `serde_json::from_str` → `WireRecommendations`; map each via `wire_rec_to_domain` → `wire_claim_to_domain`. `cited_value` via `Money::try_parse`; `cited_percentage` via `Decimal::from_str`; subject via the §0.5 `kind` mapping (`"position"`/`"buffer"`/`"net_worth"`). `prompt_hash` is left empty here (the real `GeminiAdvisor` fills it over the rendered prompt; the mock stubs it).

### mock.rs

```rust
#[derive(Default, Clone, Copy)] pub enum MockMode { #[default] Verified, Hallucinated, EmptyRecommendations }

#[derive(Debug, Clone, Copy)]
pub struct MockInvestmentAdvisor { mode: MockMode }
impl MockInvestmentAdvisor {
    pub const fn new(mode: MockMode) -> Self;
    pub const fn default_mock() -> Self;       // Verified
}
// include_str! the 3 fixtures; MOCK_MODEL_ID const.
// recommend(): serde_json::from_str through GeminiResponse -> parse_advisor_response (same path as real).
// model_id(): MOCK_MODEL_ID.
```

### Fixtures (captured Gemini-shaped JSON, deserialized through `wire.rs`)

- `gemini_verified.json` — all cited values match the canonical test snapshot exactly (AAPL $1800, buffer $5000). ≥1 verifiable recommendation.
- `gemini_hallucinated.json` — REQUIRED. Three deliberate fabrications: (a) `Position{TSLA}` not in snapshot → `UnknownTicker`; (b) `Position{AAPL}` cited `$50,000` vs truth `$1800` → `ValueMismatch`; (c) `Buffer` cited `$3000` vs truth `$5000` → `ValueMismatch`. (The §locked-decisions variant of (c) using an illegal `cited_percentage` to force `MalformedClaim` is covered by a pure-domain reconcile unit test instead; the fixture uses the wrong-figure form so it round-trips cleanly through the wire schema.)
- `gemini_empty_recs.json` — valid JSON, `"recommendations":[]` → drives `NoVerifiableInsights`.

### Phase 4 tests
- Round-trip fidelity: each fixture deserializes through `GeminiResponse` and `parse_advisor_response` succeeds (verified → ≥1 rec; hallucinated → exactly 3 recs; empty → 0 recs).
- `MockInvestmentAdvisor::model_id() == MOCK_MODEL_ID`.

---

## Phase 5 (longest pole) — reconcile + use-case + terminal states + audit logging (mock only, zero network)

### Files
```
crates/budget-app-services/src/portfolio_review/mod.rs        NEW — GeneratePortfolioReview use-case
crates/budget-app-services/src/portfolio_review/reconcile.rs  NEW — reconcile + constants
crates/budget-app-services/src/portfolio_review/reconcile/tests.rs  NEW — exhaustive reconcile tests
crates/budget-app-services/src/lib.rs                         ADD pub mod portfolio_review
crates/budget-app-services/src/error.rs                       ADD AdvisorTransport(String) variant on ServiceError
crates/budget-infrastructure/tests/advisor_mock.rs            NEW — hallucination enforcement integration test
```

### reconcile.rs — pinned tolerance constants

```rust
/// Absolute band: |cited - ground_truth| <= MONEY_BAND after round_to_cents.
/// PINNED at one cent. Implemented via a const-capable constructor so the value
/// is exact by construction (mirrors DEFAULT_DEFICIT_THRESHOLD_RATIO style):
///   Money(Decimal::from_parts(1, 0, 0, false, 2))
const MONEY_BAND: Money = /* Money::from_minor_const(1) == $0.01 */;

/// Cited "% of portfolio" precision (ratio rounded to N dp). PINNED N = 1.
const PERCENT_PRECISION_DP: u32 = 1;
```

> `MONEY_BAND` const-ness (LOCKED): `Money::from_minor` calls `Decimal::new`, which is NOT `const` in `rust_decimal` 1.x. Add `pub const fn from_minor_const(cents: i64) -> Self` to `money.rs` using `Decimal::from_parts` (exactly as `DeficitFinancingConfig` does), so `MONEY_BAND` is a genuine `const`. Confirm the `rust_decimal` version at build time; if `Decimal::new` is const there, use `from_minor` directly.

### reconcile.rs — signature + behavior

```rust
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReconcileResult {
    pub outcome: ValidationOutcome,                       // worst across claims
    pub per_claim: Vec<(ClaimSubject, ValidationOutcome)>,
}

#[must_use]
pub fn reconcile(rec: &Recommendation, snap: &PortfolioSnapshot) -> ReconcileResult;
```

`reconcile_claim` exhaustive match (no wildcard):
- **Guard (first):** `Buffer | NetWorth` with `cited_percentage.is_some()` → `Unverified(MalformedClaim)`.
- **`Position { ticker }`:** resolve in `snap.positions`; not found → `UnknownTicker`. Found but `market_value == None` → `MissingMarketData`. Else `|cited_value - market_value| > MONEY_BAND` → `ValueMismatch`. If `cited_percentage = Some(p)`: with `total_invested == 0` → `PercentageMismatch{ground_truth: 0}`; else `ground = (market_value / total_invested).round_dp(1)`, `p.round_dp(1) != ground` → `PercentageMismatch`. Else `Verified`.
- **`Buffer`:** `|cited_value - snap.buffer_total| > MONEY_BAND` → `ValueMismatch` else `Verified`.
- **`NetWorth`:** `|cited_value - snap.net_worth.total| > MONEY_BAND` → `ValueMismatch` else `Verified`.

Recommendation outcome = first `Unverified` across claims, else `Verified`. A zero-claim recommendation is `Verified` (vacuous).

### mod.rs — GeneratePortfolioReview use-case

Holds `Arc<dyn PositionSource>`, `Arc<dyn CashBalanceSource>`, `Arc<dyn MarketDataProvider>`, `Arc<dyn InvestmentAdvisor>`, `Arc<dyn ReviewRunRepository>`, `Arc<dyn UowProvider>`. Orchestrates against ports only.

```rust
pub async fn generate_portfolio_review(
    &self, user_id: UserId, now: DateTime<Utc>,
) -> Result<ReviewRun, ServiceError>;
```

Flow:
1. `try_join!(positions_for_user, balances_for_user)` (concurrent).
2. Both empty → persist `ReviewRun { terminal_state: EmptyPortfolio, recommendations: [], outcomes: [] }` WITHOUT a model call; return. (`latency_ms = 0`, tokens `None`.)
3. `try_join_all` over `market.quote(ticker)` per distinct ticker (concurrent). `None`/fail → manual fallback price or `quote: None`. Build `PricedPosition`s.
4. Assemble `PortfolioSnapshot` (`buffer_total`, `total_invested`, `NetWorth` with `liabilities = ZERO`, `captured_at = now`).
5. Measure latency around `advisor.recommend(&snapshot)`.
   - `Err(AdvisorError::Parse(raw))` → persist `MalformedOutput` with `raw_output = raw`, `recommendations = []`; return `Ok(run)`.
   - other `Err` → map to `ServiceError::AdvisorTransport`; NO run persisted (retryable transport failure).
6. `reconcile(rec, &snapshot)` for each recommendation → `outcomes: Vec<(usize, ValidationOutcome)>`.
7. Classify: zero recs OR zero verifiable → `NoVerifiableInsights`; else `Completed`.
8. Persist `ReviewRun` (model id, prompt hash, raw output, snapshot JSONB, recommendations JSONB, outcomes JSONB, tokens, latency, `occurred_at = now`) in ONE UoW transaction (`ARCH-EXPLICIT-TX-1`). Return it.

### Phase 5 tests

**reconcile/tests.rs — exhaustive (`ORCH-NEW-PATH-TESTS-1`).** Canonical `base_snapshot()`: AAPL $180×10=$1800, NVDA $500×5=$2500, total_invested $4300, buffer $5000, total_cash $6000, net_worth.total $10300.
- Position: exact verifies; within $0.01 verifies; two cents off → `ValueMismatch`; $50k hallucination → `ValueMismatch`; unknown ticker → `UnknownTicker`; quote-missing snapshot → `MissingMarketData`; correct ratio verifies; wrong ratio → `PercentageMismatch`.
- Buffer: exact verifies; within band verifies; wrong figure → `ValueMismatch`; with `cited_percentage` → `MalformedClaim`.
- NetWorth: exact verifies; wrong → `ValueMismatch`; with `cited_percentage` → `MalformedClaim`.
- Multi-claim: any unverified → recommendation `Unverified`; all three arms verified → `Verified`; per_claim length + per-index outcomes asserted.
- Zero-claim recommendation → `Verified`.

**Terminal-state tests (mod.rs or sibling tests.rs, fake ports per `deficit_financing/tests.rs`):**
- `EmptyPortfolio`: empty positions+balances → short-circuit; advisor configured to panic-if-called proves no model call.
- `MalformedOutput`: advisor `Err(Parse)` → service returns `Ok(run)`, `terminal_state == MalformedOutput`, `raw_output` non-empty.
- `NoVerifiableInsights`: zero recs; AND a separate test where recs exist but all claims `Unverified`.
- `Completed`: one verified recommendation.
- Stale quote: `quote(AAPL) -> Ok(None)`, advisor cites AAPL → run reaches a terminal state (not `EmptyPortfolio`), and `outcomes[0] == Unverified(MissingMarketData("AAPL"))` (this read requires the index-paired `outcomes` shape, §0.4).

**advisor_mock.rs integration (REQUIRED, BUDGET-AI-1 enforcement):** feed `gemini_hallucinated.json` through `MockInvestmentAdvisor(Hallucinated)`, reconcile each rec against the matching ground-truth snapshot, assert at least one `UnknownTicker`, at least one `ValueMismatch`, and that NO recommendation is `Verified`.

### Firewall note
reconcile, the exhaustive match, terminal states, and the hallucination test are fully proven against the mock here — before a single real Gemini byte in Phase 6. Do NOT cut: forced JSON, reconciliation, the audit row, the hallucination-fixture test.

---

## Phase 6 — Real GeminiAdvisor + Portfolio Review screen + review_runs mapper

### Files
```
crates/budget-infrastructure/src/advisor/gemini.rs   NEW — real HTTP adapter (reqwest + responseSchema)
crates/budget-mappers/src/review_runs.rs (or portfolio.rs)  ADD review_run_to_dto + outcome/reason/subject/terminal mappers
crates/budget-ui/src/server_state.rs                 ADD AI_MODE=mock wiring on PortfolioState::from_connections
crates/budget-ui/src/services/portfolio_review.rs    WIRE run_review body
crates/budget-ui/src/views/portfolio_review.rs       insight cards + badges + disclaimer
```

### GeminiAdvisor
`new(vault: Arc<dyn SecretVault>, model_id: String)` — `model_id` config-resolved from `GEMINI_MODEL_ID`, never hardcoded (`ORCH-TRAINING-CUTOFF-1`). Builds the prompt from the snapshot, sets `response_mime_type: application/json` + a `responseSchema` mirroring the `Claim`/`ClaimSubject` shape (subject `type` enum exactly `["position","buffer","net_worth"]`, §0.5), calls Gemini, computes `prompt_hash = sha256(rendered_prompt)`, parses via `parse_advisor_response`, returns `AdvisorOutput`. Parse failure → `AdvisorError::Parse(raw)`. Deps: `reqwest`, `sha2`, `hex`.

### AI_MODE=mock wiring (mirror PLAID_MODE=mock — STAGE-1 safety)
Only the exact string `AI_MODE=mock` selects `MockInvestmentAdvisor::default_mock()` + `MockMarketDataProvider` + in-memory vault, with a `WARN` log. Anything else / unset → real `GeminiAdvisor` + real `MarketDataProvider` + real vault (requires `KEY_VAULT_URL` + `GEMINI_MODEL_ID`, else the server fn returns 503). A misconfigured prod can never silently reach the mock.

### review_run mapper (Phase 6)
`review_run_to_dto(&ReviewRun) -> ReviewResultDto`: zips `run.recommendations[i]` with `outcomes` by index; `outcome_to_badge` renders `ValidationOutcome` → `ValidationBadgeDto`; `unverified_reason_to_string` renders each `UnverifiedReason` to a human string at THIS boundary (`RUST-DIOXUS-10`, raw codes never reach the client); `subject_to_display` renders `ClaimSubject`; `terminal_state_to_dto` maps the enum; `tax_note` computed deterministically from cited positions' `account_type` (N2), never from model output; `disclaimer = PORTFOLIO_REVIEW_DISCLAIMER`.

### run_review server fn
`require_authed_user` → `PortfolioState::extract` → `service.generate_portfolio_review(user.id(), Utc::now())` → `review_run_to_dto`. Returns `Ok(ReviewResultDto)` even for `MalformedOutput`/`EmptyPortfolio` (terminal_state communicates the outcome).

### UI (views/portfolio_review.rs)
`use_resource` for `portfolio_snapshot` + `list_positions`; read-only chorale `Table` for positions; debounced/disabled "Run Review" button; insight cards with validation badges; standing disclaimer constant always rendered.

### Phase 6 tests
- `gemini.rs` parse path against a captured real response (fixture); `prompt_hash` deterministic for a fixed prompt.
- `review_run_to_dto`: a `MalformedOutput` run renders with `recommendations: []` + the malformed terminal badge; an `Unverified` rec renders `ValidationBadgeDto::Unverified { reason }` with a human string; `tax_note` appears only for tax-advantaged `account_type`.

---

## Phase 7 (optional)
`PlaidPositionSource`, weekly auto-run + email, eval harness. Behind the same ports; no domain/wire shape changes.

---

## Terminal states (locked, each tested in Phase 5)

| state | trigger | classification | model call? | persisted? |
|---|---|---|---|---|
| `EmptyPortfolio` | no positions AND no cash | SUCCESS | no | yes |
| `MalformedOutput` | `AdvisorError::Parse` | FAILURE-of-review | yes | yes (`raw_output`) |
| `NoVerifiableInsights` | valid JSON, zero recs OR zero verifiable | SUCCESS | yes | yes |
| `Completed` | ≥1 verifiable recommendation | SUCCESS | yes | yes |
| stale/failed quote | a position's quote unresolved | NOT terminal — degrades to `quote: None` → citing claims `Unverified(MissingMarketData)` | — | — |

---

## Dependency confirmations (build time)
- `budget-infrastructure`: `reqwest`, `sha2`, `hex` (Gemini adapter + mock prompt hash).
- `budget-app-services`: `futures` (`try_join_all`).
- `money.rs`: add `from_minor_const` if `Decimal::new` is non-const in the pinned `rust_decimal`.

## Open items requiring human confirmation (`ORCH-TRAINING-CUTOFF-1`, NOT auto-decided)
1. Gemini model id + exact `responseSchema`/structured-output wire shape (JSON-in-text vs inline structured object — `wire.rs` assumes JSON-in-text; confirm against the live API). Keep model id as `GEMINI_MODEL_ID` config.
2. Market-data provider (Finnhub vs Twelve Data vs Alpha Vantage). `MockMarketDataProvider` is fully usable today under `AI_MODE=mock`; the real-path wiring in `PortfolioState::from_connections` returns `Err` until the provider is confirmed.
3. v1 net worth assets-only vs liabilities subtraction. `liabilities` reserved at `ZERO` so adding it later is not a snapshot-shape change.

---

## RESOLVED DECISIONS (Zach, 2026-06-10) — these close the three open items above

1. **Model id = config; API token = vault secret.** Add a `GEMINI_MODEL_ID` configuration setting (changeable without code). The API token is a `SecretVault` secret (Azure Key Vault in prod / `InMemorySecretVault` mock), never plaintext config/env. Build gate: the app must be wired to read *both* (model id from config, token from the vault) before the real `GeminiAdvisor` is enabled — the `from_connections` real path stays `Err` until then.
2. **Market data = multiple sources, context-bounded.** Not a single provider. `MarketDataProvider` composes as many genuinely-useful sources as practical (quote + fundamentals + recent news, etc.) to enrich the grounding snapshot — but **capped to what fits usefully in the model's context budget** (do not overflow the prompt). Choose a small set with high signal-per-token; the manual-price fallback stays for coverage gaps. (Reconciliation is unaffected — only ground-truth *prices* must reconcile; news/fundamentals are context, not citable claims unless a `ClaimSubject` variant is later added for them.)
3. **Net worth = assets-only (v1).** `sum(CashBalance) + sum(position market value)`, no liabilities subtraction. Zach's only debt is credit cards, already booked as monthly budget expenses (so already visible). `liabilities` stays reserved at `ZERO` per the design, so adding a subtraction later is not a snapshot-shape change.

**Base-app prerequisite (not part of this feature, see `DRIFT_REPORT.md`):** the income expectation is currently the `Money::ZERO` stub (`server_state.rs:223`); wiring config-driven income (B4) with the real `PaycheckConfig` is required before income/rollovers are trustworthy.
