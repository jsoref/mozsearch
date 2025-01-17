use std::collections::{BTreeMap, HashMap};
use std::env;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::blame;
use crate::file_format::analysis;
use crate::git_ops;
use crate::languages;
use crate::languages::FormatAs;
use crate::links;
use crate::tokenize;

use crate::config::GitData;
use crate::file_format::analysis::{AnalysisSource, Jump, WithLocation};
use crate::output::{self, InfoBox, Options, PanelItem, PanelSection, F};

use chrono::datetime::DateTime;
use chrono::naive::datetime::NaiveDateTime;
use chrono::offset::fixed::FixedOffset;
use git2;
use serde_json::{json, Map, Value, to_string, to_string_pretty};
use ustr::{Ustr, UstrMap};

use crate::config;

#[derive(Debug)]
pub struct FormattedLine {
    pub line: String,
    // If this line should open a new <div> and its <code> line should be position: sticky, this
    // has a String which is the symbol starting the nest.
    pub sym_starts_nest: Option<Ustr>,
    // This line should close this many <div>'s.
    pub pop_nest_count: u32,
}

/// Renders source code into a Vec of HTML-formatted lines wrapped in `FormattedLine` objects that
/// provide the metadata for the position:sticky post-processing step.  Caller is responsible
/// for generating line numbers and any blame information.
pub fn format_code(
    jumps: &UstrMap<Jump>,
    format: FormatAs,
    path: &str,
    input: &str,
    analysis: &[WithLocation<Vec<AnalysisSource>>],
) -> (Vec<FormattedLine>, String, String) {
    let tokens = match format {
        FormatAs::Binary => panic!("Unexpected binary file"),
        FormatAs::Plain => tokenize::tokenize_plain(&input),
        FormatAs::FormatCLike(spec) => tokenize::tokenize_c_like(&input, spec),
        FormatAs::FormatTagLike(script_spec) => tokenize::tokenize_tag_like(&input, script_spec),
    };

    let mut output_lines = Vec::new();
    let mut output = String::new();
    let mut last = 0;

    // The stack of AnalysisSource records that had a valid, non-redundant nesting_range.
    // (It's possible for a single source line to start multiple nesting ranges, but since our
    // use case is making the entire line position:sticky, it only makes sense to create a single
    // range in that case.)
    let mut nesting_stack: Vec<&AnalysisSource> = Vec::new();
    let mut starts_nest: Option<Ustr> = None;

    fn fixup(s: String) -> String {
        s.replace("\r", "\u{21A9}") // U+21A9 = LEFTWARDS ARROW WITH HOOK.
    }

    let mut line_start = 0;
    let mut cur_line = 1;

    let mut cur_datum = 0;

    fn entity_replace(s: String) -> String {
        s.replace("&", "&amp;").replace("<", "&lt;")
    }

    // The ANALYSIS_DATA array we output into the HTML that the "data-i" values key into.  There's
    // potentially a ton of redundant data in here.  Jumps filter out jumping to their own current
    // line, but otherwise the entries for a given symbol will be identical every time the symbol
    // is found.  This mechanism has been about binding DOM elements to structured JS data rather
    // than efficiency of encoding.
    let mut generated_json = vec![];
    // The SYM_INFO dictionary we output into the HTML.  This is used to stash the type_pretty and
    // type_sym information for symbols that possess it, including no_crossref symbols which won't
    // have an entry in `generated_json` with corresponding `data-i` attr, but do possess a
    // `data-symbols` attr (which is a more recent invention).
    let mut generated_sym_info = BTreeMap::new();

    let mut last_pos = 0;

    for token in tokens {
        //let word = &input[token.start .. token.end];
        //println!("TOK {:?} '{}' {}", token, word, last_pos);

        assert!(last_pos <= token.start);
        assert!(token.start <= token.end);
        last_pos = token.end;

        match token.kind {
            tokenize::TokenKind::Newline => {
                output.push_str(&input[last..token.start]);

                // Pop nesting symbols whose end is on the NEXT line.  That is, it doesn't make
                // sense for the position:sticky overlay to cover up the line that contains the
                // token that closes the nesting range.
                //
                // The check below accomplishes this by scanning until we find an (endline - 1)
                // that is beyond the current line.
                let truncate_to = match nesting_stack
                    .iter()
                    .rposition(|a| a.nesting_range.end_lineno - 1 > cur_line)
                {
                    Some(first_keep) => first_keep + 1,
                    None => 0,
                };
                let pop_count = nesting_stack.len() - truncate_to;
                nesting_stack.truncate(truncate_to);

                output_lines.push(FormattedLine {
                    line: fixup(output),
                    sym_starts_nest: starts_nest.take(),
                    pop_nest_count: pop_count as u32,
                });
                output = String::new();

                cur_line += 1;
                line_start = token.end;
                last = token.end;

                continue;
            }
            _ => {}
        }

        let column = (token.start - line_start) as u32;

        // Advance cur_datum as long as analysis[cur_datum] is pointing
        // to tokens we've already gone past. This effectively advances
        // cur_datum such that `analysis[cur_datum]` is the analysis data
        // for our current token (if there is any).
        while cur_datum < analysis.len() && cur_line as u32 > analysis[cur_datum].loc.lineno {
            cur_datum += 1
        }
        while cur_datum < analysis.len()
            && cur_line as u32 == analysis[cur_datum].loc.lineno
            && column > analysis[cur_datum].loc.col_start
        {
            cur_datum += 1
        }

        let datum = if cur_datum < analysis.len()
            && cur_line as u32 == analysis[cur_datum].loc.lineno
            && column == analysis[cur_datum].loc.col_start
        {
            let r = &analysis[cur_datum].data;
            cur_datum += 1;
            Some(r)
        } else {
            None
        };

        let data = match (&token.kind, datum) {
            (&tokenize::TokenKind::Identifier(None), Some(d)) |
            (&tokenize::TokenKind::StringLiteral, Some(d)) => {
                for a in d.iter() {
                    // If this symbol starts a relevant nesting range and we haven't already pushed a
                    // symbol for this line, push it onto our stack.  Note that the nesting_range
                    // identifies the start/end brace which may not be on the same line as the symbol,
                    // but since we want the symbol to be the thing that's sticky, we start the range
                    // on the symbol.
                    //
                    // A range is "relevant" if:
                    // - It has a valid nesting_range.  (Empty ranges have 0 lineno's for start/end.)
                    // - The range start is on this line or after this line.
                    // - Its end line is not on the current line or the next line and therefore will
                    //   actually trigger the "position:sticky" display scenario.
                    let nests = match (a.nesting_range.start_lineno, nesting_stack.last()) {
                        (0, _) => false,
                        (_, None) => true,
                        (a_start, Some(top)) => {
                            a_start >= cur_line
                                && a_start != top.nesting_range.start_lineno
                                && a.nesting_range.end_lineno > cur_line + 1
                        }
                    };
                    if nests {
                        starts_nest = Some(*a.sym.first().unwrap());
                        nesting_stack.push(a);
                    }

                    // Pass-through local symbol information that won't be available from the
                    // cross-reference database because it was marked no_crossref.  This is only
                    // intended to cover type information about the locals; other info like srcsym
                    // and targetsym doesn't make sense for locals.
                    if a.no_crossref && a.type_pretty.is_some() && a.sym.len() >= 1 &&
                       !generated_sym_info.contains_key(&a.sym[0]) {
                        let mut obj = Map::new();
                        if a.get_syntax_kind().is_some() {
                            obj.insert("syntax".to_string(),
                                       json!(a.get_syntax_kind().unwrap().to_string()));
                        }
                        obj.insert("type".to_string(),
                                   json!(a.type_pretty.as_ref().unwrap().to_string()));
                        if let Some(type_sym) = &a.type_sym {
                            obj.insert("typesym".to_string(), json!(type_sym.to_string()));
                        }
                        generated_sym_info.insert(a.sym[0].clone(), json!(obj));
                    }
                }

                // Build the list of symbols for the highlighter.  We do this for all source
                // records, even ones marked "no_crossref" because we still want to highlight
                // locals.  These will be emitted into a `data-symbols` attribute below.
                let syms = {
                    let mut syms = String::new();
                    for (i, sym) in d.iter().flat_map(|item| item.sym.iter()).enumerate() {
                        if i != 0 {
                            syms.push_str(",");
                        }
                        syms.push_str(sym)
                    }
                    syms
                };

                // Filter out items marked no_crossref so we can process for search/jumps for
                // context menus.
                let d = d
                    .iter()
                    .filter(|item| !item.no_crossref)
                    .collect::<Vec<_>>();

                // map to de-duplicate jumps on path:lineno, later flattened to vec.
                let mut menu_jumps: HashMap<String, Value> = HashMap::new();
                for sym in d.iter().flat_map(|item| item.sym.iter()) {
                    let jump = match jumps.get(sym) {
                        Some(jump) => jump,
                        None => continue,
                    };

                    if jump.path == *path && jump.lineno == cur_line as u64 {
                        continue;
                    }

                    menu_jumps.insert(
                        format!("{}:{}", jump.path, jump.lineno),
                        json!({
                            "sym": sym,
                            "pretty": jump.pretty,
                        })
                    );
                }

                let items = d
                    .iter()
                    .map(|item| {
                        json!({
                            "sym": item.get_joined_syms(),
                            "pretty": item.pretty,
                        })
                    })
                    .collect::<Vec<_>>();

                let menu_jumps = menu_jumps.into_iter().map(|(_, v)| v).collect::<Vec<_>>();

                let index = generated_json.len();
                if items.len() > 0 {
                    generated_json.push(json!(vec![
                        json!(menu_jumps),
                        json!(items),
                    ]));
                    format!("data-symbols=\"{}\" data-i=\"{}\" ", syms, index)
                } else {
                    format!("data-symbols=\"{}\" ", syms)
                }
            }
            _ => String::new(),
        };

        let style = match token.kind {
            tokenize::TokenKind::Identifier(None) => match datum {
                Some(d) => {
                    let classes = d.iter().flat_map(|a| {
                        a.syntax.iter().flat_map(|s| match s.as_ref() {
                            "type" => vec!["syn_type"],
                            "def" | "decl" | "idl" => vec!["syn_def"],
                            _ => vec![],
                        })
                    });
                    let classes = classes.collect::<Vec<_>>();
                    if classes.len() > 0 {
                        format!("class=\"{}\" ", classes.join(" "))
                    } else {
                        "".to_owned()
                    }
                }
                None => "".to_owned(),
            },
            tokenize::TokenKind::Identifier(Some(ref style)) => style.clone(),
            tokenize::TokenKind::StringLiteral => "class=\"syn_string\" ".to_owned(),
            tokenize::TokenKind::Comment => "class=\"syn_comment\" ".to_owned(),
            tokenize::TokenKind::TagName => "class=\"syn_tag\" ".to_owned(),
            tokenize::TokenKind::TagAttrName => "class=\"syn_tag\" ".to_owned(),
            tokenize::TokenKind::EndTagName => "class=\"syn_tag\" ".to_owned(),
            tokenize::TokenKind::RegularExpressionLiteral => "class=\"syn_regex\" ".to_owned(),
            _ => "".to_owned(),
        };

        match token.kind {
            tokenize::TokenKind::Punctuation | tokenize::TokenKind::PlainText => {
                let mut sanitized = entity_replace(input[last..token.end].to_string());
                if token.kind == tokenize::TokenKind::PlainText {
                    sanitized = links::linkify_comment(sanitized);
                }
                output.push_str(&sanitized);
                last = token.end;
            }
            _ => {
                if style != "" || data != "" {
                    output.push_str(&entity_replace(input[last..token.start].to_string()));
                    output.push_str(&format!("<span {}{}>", style, data));
                    let mut sanitized = entity_replace(input[token.start..token.end].to_string());
                    if token.kind == tokenize::TokenKind::Comment
                        || token.kind == tokenize::TokenKind::StringLiteral
                    {
                        sanitized = links::linkify_comment(sanitized);
                    }
                    output.push_str(&sanitized);
                    output.push_str("</span>");
                    last = token.end;
                }
            }
        }
    }

    output.push_str(&entity_replace(input[last..].to_string()));

    if output.len() > 0 {
        output_lines.push(FormattedLine {
            line: fixup(output),
            sym_starts_nest: starts_nest.take(),
            pop_nest_count: nesting_stack.len() as u32,
        });
    }

    let analysis_json = if env::var("MOZSEARCH_DIFFABLE").is_err() {
        to_string(&json!(generated_json)).unwrap()
    } else {
        to_string_pretty(&json!(generated_json)).unwrap()
    };
    let sym_json = if env::var("MOZSEARCH_DIFFABLE").is_err() {
        to_string(&json!(generated_sym_info)).unwrap()
    } else {
        to_string_pretty(&json!(generated_sym_info)).unwrap()
    };
    (output_lines, analysis_json, sym_json)
}

/// Renders source code with blame annotations and semantic analysis data (if provided).
/// The caller provides the panel sections.  Currently used by `output-file.rs` to statically
/// generate the tip of whatever branch it's on with semantic analysis data, and `format_path` to
/// dynamically generate the contents of a file without semantic analysis data.
pub fn format_file_data(
    cfg: &config::Config,
    tree_name: &str,
    panel: &[PanelSection],
    info_boxes: &[InfoBox],
    commit: &Option<git2::Commit>,
    blame_commit: &Option<git2::Commit>,
    path: &str,
    data: String,
    jumps: &UstrMap<Jump>,
    analysis: &[WithLocation<Vec<AnalysisSource>>],
    coverage: &Option<Vec<i32>>,
    writer: &mut dyn Write,
    mut diff_cache: Option<&mut git_ops::TreeDiffCache>,
) -> Result<(), &'static str> {
    let tree_config = cfg.trees.get(tree_name).ok_or("Invalid tree")?;

    let format = languages::select_formatting(path);
    match format {
        FormatAs::Binary => {
            write!(writer, "Binary file").unwrap();
            return Ok(());
        }
        _ => {}
    };

    let (output_lines, analysis_json, sym_json) = format_code(jumps, format, path, &data, &analysis);

    let blame_lines = git_ops::get_blame_lines(tree_config.git.as_ref(), blame_commit, path);

    let revision_owned = match commit {
        &Some(ref commit) => {
            let rev = commit.id().to_string();
            let (header, _) = blame::commit_header(commit)?;
            Some((rev, header))
        }
        &None => None,
    };
    let revision = match revision_owned {
        Some((ref rev, ref header)) => Some((rev.as_str(), header.as_str())),
        None => None,
    };

    let path_wrapper = Path::new(path);
    let filename = path_wrapper.file_name().unwrap().to_str().unwrap();

    let title = format!("{} - mozsearch", filename);
    let opt = Options {
        title: &title,
        tree_name,
        include_date: env::var("MOZSEARCH_DIFFABLE").is_err(),
        revision,
        extra_content_classes: "source-listing not-diff",
    };

    output::generate_header(&opt, writer)?;

    output::generate_breadcrumbs(&opt, writer, path)?;

    output::generate_panel(writer, panel)?;

    output::generate_info_boxes(writer, info_boxes)?;

    if let Some(ext) = path_wrapper.extension() {
        if ext.to_str().unwrap() == "svg" {
            if let Some(ref hg_root) = tree_config.paths.hg_root {
                let url = format!("{}/raw-file/tip/{}", hg_root, path);
                output::generate_svg_preview(writer, &url)?
            }
        }
    }

    let f = F::Seq(vec![F::S(
        "<div id=\"file\" class=\"file\" role=\"table\">",
    )]);

    output::generate_formatted(writer, &f, 0).unwrap();

    // Keep a cache of the "previous blame" computation across all
    // lines in the file. If we do encounter a changeset that we
    // want to ignore, chances are many lines will have that same
    // changeset as the more recent blame, and so they will all do
    // the same computation needlessly without this cache.
    let mut prev_blame_cache = git_ops::PrevBlameCache::new();

    // Map blame revisions to consecutive integer identifiers so that our aria
    // labels for screen readers can have a more human friendly identifier than
    // (some portion of) the git hash.
    let mut blame_hash_to_human_id = HashMap::new();
    let mut next_human_id = 1;

    // Blame lines and source lines are now interleaved.  Since we already have fully rendered the
    // source above, we output the blame info, line number, and rendered HTML source as we process
    // each line for blame purposes.
    let mut last_revs = None;
    let mut last_color = false;
    let mut nest_depth = 0;
    for (i, line) in output_lines.iter().enumerate() {
        let lineno = i + 1;

        // Compute the coverage data for this line (if any)
        let coverage_data: String = if let Some(ref coverage) = coverage {
            // There's 2 levels of not having data for a line here:
            // 1. We had no coverage data, coverage is None.  In that case,
            //    we'll take the else case.
            // 2. We have coverage data (coverage is Some(x)), but the array
            //    has no data for this line.  This should only happen if the
            //    coverage data is for a different revision control revision
            //    than the source code.  We map this to -4.
            //
            // We also have -3 and -2 from interpolate_coverage, and -1
            // which is directly part of the coverage data we receive (that
            // interpolation converts to -2 and -3.)
            match coverage.get(i).unwrap_or(&-4) {
                -4 => r#" class="cov-strip cov-uncovered cov-unknown" role="button" aria-label="missing data""#.to_owned(),
                -3 => r#" class="cov-strip cov-miss cov-interpolated" role="button" aria-label="uncovered""#.to_owned(),
                -2 => r#" class="cov-strip cov-hit cov-interpolated" role="button" aria-label="uncovered""#.to_owned(),
                -1 => r#" class="cov-strip cov-uncovered cov-known" role="button" aria-label="uncovered""#.to_owned(),
                 0 => r#" class="cov-strip cov-miss cov-known" role="button" aria-label="miss" data-coverage="0""#.to_owned(),
                // Should this directly be a CSS variable?
                 x => format!(
                    r#" class="cov-strip cov-hit cov-known cov-log10-{}" role="button" aria-label="hit {}{}" data-coverage="{}""#,
                    (*x as f64).log10().floor() as u32,
                    if *x < 1000 { *x } else { *x / 1000 },
                    if *x < 1000 { "" } else { "k" },
                    *x)
            }
        } else {
            " class=\"cov-strip cov-no-data\"".to_owned()
        };

        // Compute the blame data for this line (if any)
        let blame_data = if let Some(ref lines) = blame_lines {
            let blame_line = blame::LineData::deserialize(&lines[i as usize]);

            // These store the final data we ship to the front-end.
            // Each of these is a comma-separated list with one element
            // for each blame entry.
            let mut revs = blame_line.rev.to_string();
            let mut filespecs = blame_line.path.to_string();
            let mut blame_linenos = blame_line.lineno.to_string();

            let human_id =
                blame_hash_to_human_id.entry(revs.clone()).or_insert_with(|| { let id = next_human_id; next_human_id += 1; id } );

            if let Some(ref git) = tree_config.git {
                // These are the inputs to the find_prev_blame operation,
                // updated per iteration of the loop.
                let mut cur_rev = blame_line.rev.to_string();
                let mut cur_path = PathBuf::from(if blame_line.is_path_unchanged() { path } else { blame_line.path.as_ref() });
                // See bug 1670395 - if the filename has a colon in it, this code won't work properly,
                // and we might fail to parse blame_line.lineno as a u32. Guard against that case until
                // we have a proper fix, by skipping the blame-skip loop if cur_lineno is an Err value.
                let mut cur_lineno = blame_line.lineno.parse::<u32>();

                let mut max_ignored_allowed = 5; // chosen arbitrarily
                while cur_lineno.is_ok() && git.should_ignore_for_blame(&cur_rev) {
                    if max_ignored_allowed == 0 {
                        // Push an empty entry on the end to indicate we hit the
                        // limit, but the last entry was still ignored
                        revs.push_str(",");
                        filespecs.push_str(",");
                        blame_linenos.push_str(",");
                        break;
                    }
                    max_ignored_allowed -= 1;

                    let (prev_blame_line, prev_path) = match git_ops::find_prev_blame(
                        git,
                        &cur_rev,
                        &cur_path,
                        cur_lineno.unwrap(),
                        &mut prev_blame_cache,
                        diff_cache.as_mut().map(|c| &mut **c),
                    ) {
                        Ok(prev) => prev,
                        Err(e) => {
                            // This can happen for many legitimate reasons, so
                            // handle it gracefully
                            info!("Unable to find prev blame: {:?}", e);
                            break;
                        }
                    };

                    let prev_line = blame::LineData::deserialize(&prev_blame_line);

                    revs.push_str(",");
                    revs.push_str(prev_line.rev.as_ref());
                    filespecs.push_str(",");
                    filespecs.push_str(match (prev_line.is_path_unchanged(), prev_line.path.as_ref(), &prev_path, &cur_path) {
                        // file didn't move
                        (true, _, prev, cur) if prev == cur => "%",
                        // file moved
                        (true, _, prev, _) => prev.to_str().unwrap(),
                        // file moved, then moved back
                        (false, prevprev, _, cur) if Path::new(prevprev) == *cur => "%",
                        // file moved and moved again
                        (false, prevprev, _, _) => prevprev,
                    });

                    blame_linenos.push_str(",");
                    blame_linenos.push_str(prev_line.lineno.as_ref());

                    // Update inputs to find_prev_blame for the next iteration
                    cur_rev = prev_line.rev.to_string();
                    cur_path = if prev_line.is_path_unchanged() {
                        prev_path
                    } else {
                        PathBuf::from(prev_line.path.as_ref())
                    };
                    cur_lineno = prev_line.lineno.parse::<u32>();
                }
            }

            let same_rev_as_last = last_revs.map_or(false, |last| last == revs);
            let color = if same_rev_as_last {
                last_color
            } else {
                !last_color
            };
            last_revs = Some(revs.clone());
            last_color = color;
            let class = if color { 1 } else { 2 };
            let data = format!(
                r#" class="blame-strip c{}" data-blame="{}#{}#{}" role="button" aria-label="{}" aria-expanded="false""#,
                class, revs, filespecs, blame_linenos,
                format!(
                    "{} hash {}",
                    if same_rev_as_last { "same" } else { "new" },
                    human_id
                )
            );
            data
        } else {
            " class=\"blame-strip\"".to_owned()
        };

        // If this line starts nesting, we need to create a div that exists strictly to contain the
        // position:sticky element.
        if let Some(nest_sym) = &line.sym_starts_nest {
            write!(
                writer,
                r#"<div class="nesting-container nesting-depth-{}" data-nesting-sym="{}">"#,
                nest_depth,
                nest_sym
            ).unwrap();
            nest_depth += 1;
        }

        // Emit the actual source line here.
        let f = F::Seq(vec![
            F::T(format!(
                "<div role=\"row\" id=\"line-{}\" class=\"source-line-with-number{}\">",
                lineno,
                if line.sym_starts_nest.is_some() { " nesting-sticky-line" } else { "" }
            )),
            F::Indent(vec![
                // Coverage Info. Its contents go in a div nested inside the
                // "cell" role div because in order to make the hover UI
                // accessible we expose it as a role=button which needs its own
                // element.
                F::T(format!(
                    "<div role=\"cell\"><div{}></div></div>",
                    coverage_data
                )),
                // Blame info.  Contents are nested for the exact same reason as
                // the coverage info (role=button needs its own div).
                F::T(format!(
                    "<div role=\"cell\"><div{}></div></div>",
                    blame_data
                )),
                // The line number.
                F::T(format!(
                    "<div role=\"cell\" class=\"line-number\" data-line-number=\"{}\"></div>",
                    lineno
                )),
                // The source line.
                F::T(format!(
                    "<code role=\"cell\" class=\"source-line\">{}\n</code>",
                    line.line
                )),
            ]),
            F::S("</div>"),
        ]);
        output::generate_formatted(writer, &f, 0).unwrap();

        // And at the end of this line we need to pop off the appropriate number of position:sticky
        // containing elements.
        for _ in 0..line.pop_nest_count {
            nest_depth -= 1;
            write!(writer, "</div>").unwrap();
        }
    }

    let f = F::Seq(vec![F::S("</div>")]);
    output::generate_formatted(writer, &f, 0).unwrap();

    write!(
        writer,
        "<script>var ANALYSIS_DATA = {}; var SYM_INFO = {};</script>\n",
        analysis_json,
        sym_json,
    )
    .unwrap();

    output::generate_footer(&opt, tree_name, path, writer).unwrap();

    Ok(())
}

fn entry_to_blob(repo: &git2::Repository, entry: &git2::TreeEntry) -> Result<String, &'static str> {
    match entry.kind() {
        Some(git2::ObjectType::Blob) => {}
        _ => return Err("Invalid path; expected file"),
    }

    if entry.filemode() == 120000 {
        return Err("Path is to a symlink");
    }

    Ok(git_ops::read_blob_entry(repo, entry))
}

/// Dynamically renders the contents of a specific file with blame annotations but without any
/// semantic analysis data available.  Used by the "rev" display and the "diff" mechanism when
/// there aren't actually any changes in the diff.
pub fn format_path(
    cfg: &config::Config,
    tree_name: &str,
    rev: &str,
    path: &str,
    writer: &mut dyn Write,
) -> Result<(), &'static str> {
    // Get the file data.
    let tree_config = cfg.trees.get(tree_name).ok_or("Invalid tree")?;
    let git = config::get_git(tree_config)?;
    let commit_obj = git.repo.revparse_single(rev).map_err(|_| "Bad revision")?;
    let commit = commit_obj.into_commit().map_err(|_| "Bad revision")?;
    let commit_tree = commit.tree().map_err(|_| "Bad revision")?;
    let path_obj = Path::new(path);
    let data = match commit_tree.get_path(path_obj) {
        Ok(entry) => entry_to_blob(&git.repo, &entry)?,
        Err(_) => {
            // Check to see if this path is inside a submodule
            let mut test_path = path_obj.parent();
            loop {
                let subrepo_path = match test_path {
                    Some(path) => path,
                    None => return Err("File not found"),
                };
                let entry = match commit_tree.get_path(subrepo_path) {
                    Ok(e) => e,
                    Err(_) => {
                        test_path = subrepo_path.parent();
                        continue;
                    }
                };
                if entry.kind() != Some(git2::ObjectType::Commit) {
                    return Err("File not found");
                }

                // If we get here, the path is inside a submodule
                let subrepo_path = subrepo_path.to_str().ok_or("UTF-8 error")?;
                let subrepo = git
                    .repo
                    .find_submodule(subrepo_path)
                    .map_err(|_| "Can't find submodule")?;
                let subrepo = subrepo.open().map_err(|_| "Can't open submodule")?;
                let path_in_subrepo = path_obj
                    .strip_prefix(subrepo_path)
                    .map_err(|_| "Submodule path error")?;
                let subentry = subrepo
                    .find_commit(entry.id())
                    .and_then(|commit| commit.tree())
                    .and_then(|tree| tree.get_path(path_in_subrepo))
                    .map_err(|_| "File not found in submodule")?;
                break entry_to_blob(&subrepo, &subentry)?;
            }
        }
    };

    // Get blame.
    let blame_commit = if let Some(ref blame_repo) = git.blame_repo {
        let blame_oid = git
            .blame_map
            .get(&commit.id())
            .ok_or("Unable to find blame for revision")?;
        Some(
            blame_repo
                .find_commit(*blame_oid)
                .map_err(|_| "Blame is not a blob")?,
        )
    } else {
        None
    };

    let jumps: UstrMap<analysis::Jump> = UstrMap::default();
    let analysis = Vec::new();

    let hg_rev: &str = tree_config
        .git
        .as_ref()
        .and_then(|git| git.hg_map.get(&commit.id()))
        .and_then(|rev| Some(rev.as_ref())) // &String to &str conversion
        .unwrap_or(&"tip");

    let mut vcs_panel_items = vec![];
    vcs_panel_items.push(PanelItem {
        title: "Go to latest version".to_owned(),
        link: format!("/{}/source/{}", tree_name, path),
        update_link_lineno: "#{}",
        accel_key: None,
        copyable: true,
    });
    if let Some(ref hg_root) = tree_config.paths.hg_root {
        vcs_panel_items.push(PanelItem {
            title: "Log".to_owned(),
            link: format!("{}/log/{}/{}", hg_root, hg_rev, path),
            update_link_lineno: "",
            accel_key: Some('L'),
            copyable: true,
        });
        vcs_panel_items.push(PanelItem {
            title: "Raw".to_owned(),
            link: format!("{}/raw-file/{}/{}", hg_root, hg_rev, path),
            update_link_lineno: "",
            accel_key: Some('R'),
            copyable: true,
        });
    }
    if tree_config.paths.git_blame_path.is_some() {
        vcs_panel_items.push(PanelItem {
            title: "Blame".to_owned(),
            link:
                "javascript:alert('Hover over the gray bar on the left to see blame information.')"
                    .to_owned(),
            update_link_lineno: "",
            accel_key: None,
            copyable: false,
        });
    }
    let panel = vec![PanelSection {
        name: "Revision control".to_owned(),
        items: vcs_panel_items,
    }];

    format_file_data(
        cfg,
        tree_name,
        &panel,
        &vec![],
        &Some(commit),
        &blame_commit,
        path,
        data,
        &jumps,
        &analysis,
        &None,
        writer,
        None,
    )
}

fn split_lines(s: &str) -> Vec<&str> {
    let mut split = s.split('\n').collect::<Vec<_>>();
    if split[split.len() - 1].len() == 0 {
        split.pop();
    }
    split
}

/// Dynamically renders a specific diff with blame annotations but without any semantic analysis
/// data available.
pub fn format_diff(
    cfg: &config::Config,
    tree_name: &str,
    rev: &str,
    path: &str,
    writer: &mut dyn Write,
) -> Result<(), &'static str> {
    let tree_config = cfg.trees.get(tree_name).ok_or("Invalid tree")?;

    let git_path = config::get_git_path(tree_config)?;
    let output = Command::new("/usr/bin/git")
        .arg("diff-tree")
        .arg("-p")
        .arg("--cc")
        .arg("--patience")
        .arg("--full-index")
        .arg("--no-prefix")
        .arg("-U100000")
        .arg(rev)
        .arg("--")
        .arg(path)
        .current_dir(&git_path)
        .output()
        .map_err(|_| "Diff failed 1")?;
    if !output.status.success() {
        println!("ERR\n{}", git_ops::decode_bytes(output.stderr));
        return Err("Diff failed 2");
    }
    let difftxt = git_ops::decode_bytes(output.stdout);

    if difftxt.len() == 0 {
        return format_path(cfg, tree_name, rev, path, writer);
    }

    let git = config::get_git(tree_config)?;
    let commit_obj = git.repo.revparse_single(rev).map_err(|_| "Bad revision")?;
    let commit = commit_obj.as_commit().ok_or("Bad revision")?;

    let mut blames = Vec::new();

    for parent_oid in commit.parent_ids() {
        let blame_repo = match git.blame_repo {
            Some(ref r) => r,
            None => {
                blames.push(None);
                continue;
            }
        };

        let blame_oid = git
            .blame_map
            .get(&parent_oid)
            .ok_or("Unable to find blame")?;
        let blame_commit = blame_repo
            .find_commit(*blame_oid)
            .map_err(|_| "Blame is not a blob")?;
        let blame_tree = blame_commit.tree().map_err(|_| "Bad revision")?;
        match blame_tree.get_path(Path::new(path)) {
            Ok(blame_entry) => {
                let blame = git_ops::read_blob_entry(blame_repo, &blame_entry);
                let blame_lines = blame.lines().map(|s| s.to_owned()).collect::<Vec<_>>();
                blames.push(Some(blame_lines));
            }
            Err(_) => {
                blames.push(None);
            }
        }
    }

    let mut new_lineno = 1;
    let mut old_lineno = commit.parent_ids().map(|_| 1).collect::<Vec<_>>();

    let mut lines = split_lines(&difftxt);
    for i in 0..lines.len() {
        if lines[i].starts_with('@') && i + 1 < lines.len() {
            lines = lines.split_off(i + 1);
            break;
        }
    }

    let mut new_lines = String::new();

    let mut output = Vec::new();
    for line in lines {
        if line.len() == 0 || line.starts_with('\\') {
            continue;
        }

        let num_parents = commit.parents().count();
        let (origin, content) = line.split_at(num_parents);
        let origin = origin.chars().collect::<Vec<_>>();
        let mut cur_blame = None;
        for i in 0..num_parents {
            let has_minus = origin.contains(&'-');
            if (has_minus && origin[i] == '-') || (!has_minus && origin[i] != '+') {
                cur_blame = match blames[i] {
                    Some(ref lines) => Some(&lines[old_lineno[i] - 1]),
                    None => return Err("expected blame for '-' line, none found"),
                };
                old_lineno[i] += 1;
            }
        }

        let mut lno = -1;
        if !origin.contains(&'-') {
            new_lines.push_str(content);
            new_lines.push('\n');

            lno = new_lineno;
            new_lineno += 1;
        }

        output.push((lno, cur_blame, origin, content));
    }

    let format = languages::select_formatting(path);
    match format {
        FormatAs::Binary => {
            return Err("Cannot diff binary file");
        }
        _ => {}
    };
    let jumps: UstrMap<analysis::Jump> = UstrMap::default();
    let analysis = Vec::new();
    let (formatted_lines, _, _) = format_code(&jumps, format, path, &new_lines, &analysis);

    let (header, _) = blame::commit_header(&commit)?;

    let filename = Path::new(path).file_name().unwrap().to_str().unwrap();
    let title = format!("{} - mozsearch", filename);
    let opt = Options {
        title: &title,
        tree_name,
        include_date: true,
        revision: Some((rev, &header)),
        extra_content_classes: "source-listing diff",
    };

    output::generate_header(&opt, writer)?;

    let mut vcs_panel_items = vec![
        PanelItem {
            title: "Show changeset".to_owned(),
            link: format!("/{}/commit/{}", tree_name, rev),
            update_link_lineno: "",
            accel_key: None,
            copyable: true,
        },
        PanelItem {
            title: "Show file without diff".to_owned(),
            link: format!("/{}/rev/{}/{}", tree_name, rev, path),
            update_link_lineno: "#{}",
            accel_key: None,
            copyable: true,
        },
        PanelItem {
            title: "Go to latest version".to_owned(),
            link: format!("/{}/source/{}", tree_name, path),
            update_link_lineno: "#{}",
            accel_key: None,
            copyable: false,
        },
    ];
    if let Some(ref hg_root) = tree_config.paths.hg_root {
        vcs_panel_items.push(PanelItem {
            title: "Log".to_owned(),
            link: format!("{}/log/tip/{}", hg_root, path),
            update_link_lineno: "",
            accel_key: Some('L'),
            copyable: true,
        });
    }
    let sections = vec![PanelSection {
        name: "Revision control".to_owned(),
        items: vcs_panel_items,
    }];
    output::generate_panel(writer, &sections)?;

    let f = F::Seq(vec![F::S(
        "<div id=\"file\" class=\"file\" role=\"table\">",
    )]);

    output::generate_formatted(writer, &f, 0).unwrap();

    fn entity_replace(s: String) -> String {
        s.replace("&", "&amp;").replace("<", "&lt;")
    }

    let mut last_rev = String::new();
    let mut last_color = false;
    for &(lineno, blame, ref origin, content) in &output {
        let blame_data = match blame {
            Some(blame) => {
                let line_data = blame::LineData::deserialize(blame);

                let color = if last_rev == line_data.rev {
                    last_color
                } else {
                    !last_color
                };
                last_rev = line_data.rev.to_string();
                last_color = color;
                let class = if color { 1 } else { 2 };
                format!(
                    r#" class="blame-strip c{}" data-blame="{}#{}#{}" role="button" aria-label="blame" aria-expanded="false""#,
                    class, line_data.rev, line_data.path, line_data.lineno
                )
            }
            None => " class=\"blame-strip\"".to_owned(),
        };

        let content = entity_replace(content.to_owned());
        let content = if lineno > 0 && (lineno as usize) < formatted_lines.len() + 1 {
            &formatted_lines[(lineno as usize) - 1].line
        } else {
            &content
        };

        let origin = origin.iter().cloned().collect::<String>();

        let class = if origin.contains('-') {
            " minus-line"
        } else if origin.contains('+') {
            " plus-line"
        } else {
            ""
        };

        let f = F::Seq(vec![
            F::T(format!(
                "<div role=\"row\" id=\"line-{}\" class=\"source-line-with-number\">",
                // note: this can be -1 but that's the way it's always been.
                lineno
            )),
            F::Indent(vec![
                // Coverage info.
                F::T(format!(
                    "<div role=\"cell\" class=\"blame-container\"><div{}></div></div>",
                    blame_data
                )),
                // Blame info.
                F::T(format!(
                    "<div role=\"cell\" class=\"blame-container\"><div{}></div></div>",
                    blame_data
                )),
                // The line number.
                F::T(format!(
                    "<div role=\"cell\" class=\"line-number\" data-line-number=\"{}\"></div>",
                    if lineno > 0 {
                        format!("{}", lineno)
                    } else {
                        "".to_owned()
                    },
                )),
                // The source line.
                F::T(format!(
                    "<code role=\"cell\" class=\"source-line{}\">{} {}\n</code>",
                    class, origin, content
                )),
            ]),
            F::S("</div>"),
        ]);

        output::generate_formatted(writer, &f, 0).unwrap();
    }

    let f = F::Seq(vec![F::S("</div>")]);
    output::generate_formatted(writer, &f, 0).unwrap();

    output::generate_footer(&opt, tree_name, path, writer).unwrap();

    Ok(())
}

fn generate_commit_info(
    tree_name: &str,
    tree_config: &config::TreeConfig,
    writer: &mut dyn Write,
    commit: &git2::Commit,
) -> Result<(), &'static str> {
    let (header, remainder) = blame::commit_header(&commit)?;

    fn format_rev(tree_name: &str, oid: git2::Oid) -> String {
        format!("<a href=\"/{}/commit/{}\">{}</a>", tree_name, oid, oid)
    }

    fn format_sig(sig: git2::Signature, git: &GitData) -> String {
        let (name, email) = git
            .mailmap
            .lookup(sig.name().unwrap(), sig.email().unwrap());
        format!("{} &lt;{}>", name, email)
    }

    let parents = commit
        .parent_ids()
        .map(|p| {
            F::T(format!(
                "<tr><td>parent</td><td>{}</td></tr>",
                format_rev(tree_name, p)
            ))
        })
        .collect::<Vec<_>>();

    let git = config::get_git(tree_config)?;
    let hg = match git.hg_map.get(&commit.id()) {
        Some(hg_id) => {
            let hg_link = format!(
                "<a href=\"{}/rev/{}\">{}</a>",
                tree_config.paths.hg_root.as_ref().unwrap(),
                hg_id,
                hg_id
            );
            vec![F::T(format!("<tr><td>hg</td><td>{}</td></tr>", hg_link))]
        }

        None => vec![],
    };

    let id_string = format!("{}", commit.id());
    let gitstr = tree_config.paths.github_repo.as_ref().map(|ref ghurl| {
        format!(
            "<a href=\"{}/commit/{}\">{}</a>",
            ghurl, id_string, id_string
        )
    });

    let naive_t = NaiveDateTime::from_timestamp(commit.time().seconds(), 0);
    let tz = FixedOffset::east(commit.time().offset_minutes() * 60);
    let t: DateTime<FixedOffset> = DateTime::from_utc(naive_t, tz);
    let t = t.to_rfc2822();

    let f = F::Seq(vec![
        F::T(format!("<h3>{}</h3>", header)),
        F::T(format!("<pre><code>{}</code></pre>", remainder)),
        F::S("<table>"),
        F::Indent(vec![
            F::T(format!(
                "<tr><td>commit</td><td>{}</td></tr>",
                format_rev(tree_name, commit.id())
            )),
            F::Seq(parents),
            F::Seq(hg),
            F::T(gitstr.map_or(String::new(), |g| {
                format!("<tr><td>git</td><td>{}</td></tr>", g)
            })),
            F::T(format!(
                "<tr><td>author</td><td>{}</td></tr>",
                format_sig(commit.author(), git)
            )),
            F::T(format!(
                "<tr><td>committer</td><td>{}</td></tr>",
                format_sig(commit.committer(), git)
            )),
            F::T(format!("<tr><td>commit time</td><td>{}</td></tr>", t)),
        ]),
        F::S("</table>"),
    ]);

    output::generate_formatted(writer, &f, 0)?;

    let git_path = config::get_git_path(tree_config)?;
    let output = Command::new("/usr/bin/git")
        .arg("show")
        .arg("--cc")
        .arg("--pretty=format:")
        .arg("--raw")
        .arg(id_string)
        .current_dir(&git_path)
        .output()
        .map_err(|_| "Diff failed 1")?;
    if !output.status.success() {
        println!("ERR\n{}", git_ops::decode_bytes(output.stderr));
        return Err("Diff failed 2");
    }
    let difftxt = git_ops::decode_bytes(output.stdout);

    let lines = split_lines(&difftxt);
    let mut changes = Vec::new();
    for line in lines {
        if line.len() == 0 {
            continue;
        }

        let suffix = &line[commit.parents().count()..];
        let prefix_size = 2 * (commit.parents().count() + 1);
        let mut data = suffix.splitn(prefix_size + 1, ' ');
        let data = data.nth(prefix_size).ok_or("Invalid diff output 3")?;
        let file_info = data.split('\t').take(2).collect::<Vec<_>>();

        let f = F::T(format!(
            "<li>{} <a href=\"/{}/diff/{}/{}\">{}</a>",
            file_info[0],
            tree_name,
            commit.id(),
            file_info[1],
            file_info[1]
        ));
        changes.push(f);
    }

    let f = F::Seq(vec![F::S("<ul>"), F::Indent(changes), F::S("</ul>")]);
    output::generate_formatted(writer, &f, 0)?;

    Ok(())
}

pub fn format_commit(
    cfg: &config::Config,
    tree_name: &str,
    rev: &str,
    writer: &mut dyn Write,
) -> Result<(), &'static str> {
    let tree_config = cfg.trees.get(tree_name).ok_or("Invalid tree")?;

    let git = config::get_git(tree_config)?;
    let commit_obj = git.repo.revparse_single(rev).map_err(|_| "Bad revision")?;
    let commit = commit_obj.as_commit().ok_or("Bad revision")?;

    let title = format!("{} - mozsearch", rev);
    let opt = Options {
        title: &title,
        tree_name: tree_name,
        include_date: true,
        revision: None,
        extra_content_classes: "commit",
    };

    output::generate_header(&opt, writer)?;

    generate_commit_info(tree_name, &tree_config, writer, commit)?;

    output::generate_footer(&opt, tree_name, "", writer).unwrap();

    Ok(())
}
