extern crate clap;
extern crate structopt;

pub mod builder;
pub mod interface;
pub mod parser;

mod cmd_crossref_lookup;
mod cmd_filter_analysis;
mod cmd_merge_analyses;
mod cmd_prod_filter;
mod cmd_query;
mod cmd_search_identifiers;
mod cmd_show_html;

pub use builder::{build_pipeline};
pub use interface::{PipelineCommand, PipelineValues};
