//! Investment-advisor adapters for AI Portfolio Insights
//! (`docs/AI_FEATURE_DESIGN.md §Phase 4`).
//!
//! - [`wire`] — the Gemini wire DTOs + `parse_advisor_response`, the single
//!   wire→domain boundary (§0.3/§0.5). `pub(crate)`: never crosses the crate
//!   surface (the domain types do).
//! - [`mock`] — [`MockInvestmentAdvisor`], the fixture-driven advisor that parses
//!   captured Gemini-shaped JSON through the SAME `parse_advisor_response` path
//!   the real (Phase-6) adapter will. The whole reconciliation firewall is proven
//!   against this mock before any real Gemini byte.
//!
//! - [`gemini`] — the real [`GeminiAdvisor`] HTTP adapter (Phase 6): builds the
//!   grounding prompt, calls Google's Generative Language `generateContent`
//!   endpoint with the §0.5 `responseSchema`, hashes the prompt, and parses the
//!   response through the SAME `parse_advisor_response` path the mock uses.

pub mod gemini;
pub mod mock;
pub(crate) mod wire;

pub use gemini::{GEMINI_API_KEY_SECRET, GeminiAdvisor};
pub use mock::{MOCK_MODEL_ID, MockInvestmentAdvisor, MockMode};
