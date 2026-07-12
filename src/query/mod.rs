//! SQL query handling: parsing and execution.

pub mod executor;
pub mod parser;

pub use executor::{Executor, QueryResult};
pub use parser::{parse, Statement};
