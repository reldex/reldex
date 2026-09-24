//! Vendor-neutral SQL/PL-SQL lexing and statement splitting for Reldex.
//!
//! `SPEC.md` §15 forbids splitting a script by every semicolon: the editor
//! must understand SQL and PL/SQL block boundaries and SQL\*Plus's `/`. This
//! crate is where that understanding lives, and it lives here — not in
//! `db-core`, not in a driver, and not in the Qt adapter — because splitting
//! and highlighting are the *same* problem (both have to know where a string,
//! a comment, and a block statement begin and end) and neither one is
//! specific to Oracle. What *is* specific to Oracle (which keywords open a
//! block, whether `/` ends one, which quoting forms exist) is data, supplied
//! by the driver as a [`SqlDialect`] value. This crate contains no database
//! vendor's name in its logic; every dialect-specific fact arrives through
//! that one parameter.
//!
//! # Shape of the crate
//!
//! ```text
//! SqlDialect (driver-supplied data)
//!        │
//!        ├── tokenize / tokenize_block  ──▶ Vec<Token>   (lexer.rs)
//!        │        (QSyntaxHighlighter-shaped: one call per text block,
//!        │         a small Copy LexState carried across blocks)
//!        │
//!        └── split_statements / statement_at ──▶ Vec<StatementSpan>
//!                 (splitter.rs; built on top of `tokenize`, not a separate
//!                  scan, so "what counts as a string/comment" is answered
//!                  exactly once)
//! ```
//!
//! # Dependencies
//!
//! None, deliberately (`AGENTS.md`, "Do not add production dependencies
//! without documenting why they are required"; `docs/exec-plans/active/`
//! constraint for this task). The lexer is a hand-written character-by-
//! character scanner — no regex engine — because the token grammar is small,
//! fixed by [`SqlDialect`] data, and byte/line-oriented in a way a general
//! regex engine would not simplify.
//!
//! # Where `SqlDialect` lives, and why
//!
//! `docs/exec-plans/active/phase-1.md` §B4 sketches this descriptor as
//! "additive, in `db-core` rather than the contract". This crate places it
//! here instead — see the crate's own module doc on [`dialect`] for the
//! reasoning and the ADR-0002 amendment this task drafts.
//!
//! # What lives elsewhere
//!
//! Converting a byte offset to a UTF-16 offset (Qt's own unit) is already
//! solved once, generically, at the FFI boundary:
//! `crates::ffi::reldex_utf16_offset`. This crate stays byte-oriented and
//! reports line/column in **Unicode scalar values** (`char` count), not
//! UTF-16 code units; a caller that needs a `QTextCursor` column converts
//! with that existing utility rather than this crate duplicating it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(unreachable_pub)]

pub mod dialect;
pub mod lexer;
pub mod splitter;
pub mod token;

pub use dialect::{
    BlockKind, BlockStarter, CommentRules, KeywordSlot, Phrase, QuotingRules, SqlDialect,
};
pub use lexer::{tokenize, tokenize_block};
pub use splitter::{EndedBy, StatementKind, StatementSpan, split_statements, statement_at};
pub use token::{LexState, Token, TokenKind};
