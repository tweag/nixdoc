//! This tool generates DocBook XML from a Nix file defining library
//! functions, such as the files in `lib/` in the nixpkgs repository.
//!
//! TODO:
//! * extract function argument names
//! * extract line number & add it to generated output
//! * figure out how to specify examples (& leading whitespace?!)

use failure::Error;
use rnix::parser::{Arena, ASTNode, ASTKind, Data};
use rnix::tokenizer::Meta;
use rnix::tokenizer::Trivia;
use rnix;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;
use structopt::StructOpt;
use xml::writer::{EventWriter, EmitterConfig, XmlEvent};

type Result<T> = std::result::Result<T, Error>;

/// Command line arguments for nixdoc
#[derive(Debug, StructOpt)]
#[structopt(name = "nixdoc", about = "Generate Docbook from Nix library functions")]
struct Options {
    /// Nix file to process.
    #[structopt(short = "f", long = "file", parse(from_os_str))]
    file: PathBuf,

    /// Name of the function category (e.g. 'strings', 'attrsets').
    #[structopt(short = "c", long = "category")]
    category: String,

    /// Description of the function category.
    #[structopt(short = "d", long = "description")]
    description: String,
}

#[derive(Debug)]
struct DocComment {
    /// Primary documentation string.
    doc: String,

    /// Optional type annotation for the thing being documented.
    doc_type: Option<String>,

    /// Usage example(s) (interpreted as a single code block)
    example: Option<String>,
}

#[derive(Debug)]
struct DocItem {
    name: String,
    comment: DocComment,
    args: Vec<String>,
}

/// Represents a single manual section describing a library function.
#[derive(Debug)]
struct ManualEntry {
    /// Name of the function category (e.g. 'strings', 'trivial', 'attrsets')
    category: String,

    /// Name of the section (used as the title)
    name: String,

    /// Type signature (if provided). This is not actually a checked
    /// type signature in any way.
    fn_type: Option<String>,

    /// Primary description of the entry. Each entry is written as a
    /// separate paragraph.
    description: Vec<String>,

    /// Usage example for the entry.
    example: Option<String>,

    /// Arguments of the function
    args: Vec<String>,
}

impl ManualEntry {
    /// Write a single DocBook entry for a documented Nix function.
    fn write_section_xml<W: Write>(&self, w: &mut EventWriter<W>) -> Result<()> {
        let ident = format!("lib.{}.{}", self.category, self.name);

        // <section ...
        w.write(XmlEvent::start_element("section")
                .attr("xml:id", format!("function-library-{}", ident).as_str()))?;

        // <title> ...
        w.write(XmlEvent::start_element("title"))?;
        w.write(XmlEvent::start_element("function"))?;
        w.write(XmlEvent::characters(ident.as_str()))?;
        w.write(XmlEvent::end_element())?;
        w.write(XmlEvent::end_element())?;

        // <subtitle> (type signature)
        if let Some(t) = &self.fn_type {
            w.write(XmlEvent::start_element("subtitle"))?;
            w.write(XmlEvent::start_element("literal"))?;
            w.write(XmlEvent::characters(t))?;
            w.write(XmlEvent::end_element())?;
            w.write(XmlEvent::end_element())?;
        }

        // Include link to function location (location information is
        // generated by a separate script in nixpkgs)
        w.write(XmlEvent::start_element("xi:include")
                .attr("href", "./locations.xml")
                .attr("xpointer", &ident))?;
        w.write(XmlEvent::end_element())?;

        // Primary doc string
        // TODO: Split paragraphs?
        for paragraph in &self.description {
            w.write(XmlEvent::start_element("para"))?;
            w.write(XmlEvent::characters(paragraph))?;
            w.write(XmlEvent::end_element())?;
        }

        // Function argument names
        if !self.args.is_empty() {
            w.write(XmlEvent::start_element("variablelist"))?;
            for arg in &self.args {
                w.write(XmlEvent::start_element("varlistentry"))?;

                w.write(XmlEvent::start_element("term"))?;
                w.write(XmlEvent::start_element("varname"))?;
                w.write(XmlEvent::characters(arg))?;
                w.write(XmlEvent::end_element())?;
                w.write(XmlEvent::end_element())?;

                w.write(XmlEvent::start_element("listitem"))?;
                w.write(XmlEvent::start_element("para"))?;
                w.write(XmlEvent::characters("Function argument"))?;
                w.write(XmlEvent::end_element())?;
                w.write(XmlEvent::end_element())?;

                w.write(XmlEvent::end_element())?;
            }

            w.write(XmlEvent::end_element())?;
        }

        // Example program listing (if applicable)
        //
        // TODO: In grhmc's version there are multiple (named)
        // examples, how can this be achieved automatically?
        if let Some(example) = &self.example {
            w.write(XmlEvent::start_element("example"))?;

            w.write(XmlEvent::start_element("title"))?;

            w.write(XmlEvent::start_element("function"))?;
            w.write(XmlEvent::characters(ident.as_str()))?;
            w.write(XmlEvent::end_element())?;

            w.write(XmlEvent::characters(" usage example"))?;
            w.write(XmlEvent::end_element())?;

            w.write(XmlEvent::start_element("programlisting"))?;
            w.write(XmlEvent::cdata(example))?;
            w.write(XmlEvent::end_element())?;

            w.write(XmlEvent::end_element())?;
        }

        // </section>
        w.write(XmlEvent::end_element())?;

        Ok(())
    }
}

/// Retrieve documentation comments. For now only multiline comments
/// starting with `@doc` are considered.
fn retrieve_doc_comment(meta: &Meta) -> Option<String> {
    for item in meta.leading.iter() {
        if let Trivia::Comment { multiline, content, .. } = item {
            if *multiline { //  && content.as_str().starts_with(" @doc") {
                return Some(content.to_string())
            }
        }
    }

    return None;
}

/// Transforms an AST node into a `DocItem` if it has a leading
/// documentation comment.
fn retrieve_doc_item(node: &ASTNode) -> Option<DocItem> {
    // We are only interested in identifiers.
    if let Data::Ident(meta, name) = &node.data {
        let comment = retrieve_doc_comment(meta)?;

        return Some(DocItem {
            name: name.to_string(),
            comment: parse_doc_comment(&comment),
            args: vec![],
        })
    }

    return None;
}

/// *Really* dumb, mutable, hacky doc comment "parser".
fn parse_doc_comment(raw: &str) -> DocComment {
    enum ParseState { Doc, Type, Example }

    let mut doc = String::new();
    let mut doc_type = String::new();
    let mut example = String::new();
    let mut state = ParseState::Doc;

    for line in raw.trim().lines() {
        let mut line = line.trim();

        if line.starts_with("@doc ") {
            state = ParseState::Doc;
            line = line.trim_start_matches("@doc ");
        }

        if line.starts_with("Type:") {
            state = ParseState::Type;
            line = &line[5..]; //.trim_start_matches("Type:");
        }

        if line.starts_with("Example:") {
            state = ParseState::Example;
            line = line.trim_start_matches("Example:");
        }

        match state {
            ParseState::Type => doc_type.push_str(line.trim()),
            ParseState::Doc => {
                doc.push_str(line.trim());
                doc.push('\n');
            },
            ParseState::Example => {
                example.push_str(line.trim());
                example.push('\n');
            },
        }
    }

    let f = |s: String| if s.is_empty() { None } else { Some(s.into()) };

    DocComment {
        doc: doc.trim().into(),
        doc_type: f(doc_type),
        example: f(example),
    }
}

/// Traverse a Nix lambda and collect the identifiers of arguments
/// until an unexpected AST node is encountered.
///
/// This will collect the argument names for curried functions in the
/// `a: b: c: ...`-style, but does not currently work with pattern
/// functions (`{ a, b, c }: ...`).
///
/// In the AST representation used by rnix, any lambda node has an
/// immediate child that is the identifier of its argument. The "body"
/// of the lambda is two steps to the right from that identifier, if
/// it is a lambda the function is curried and we can recurse.
fn collect_lambda_args<'a>(arena: &Arena<'a>,
                           lambda_node: &ASTNode,
                           args: &mut Vec<String>) -> Option<()> {
    let ident_node = &arena[lambda_node.node.child?];
    if let Data::Ident(_, name) = &ident_node.data {
        args.push(name.to_string());
    }

    // Two to the right ...
    let token_node = &arena[ident_node.node.sibling?];
    let body_node = &arena[token_node.node.sibling?];

    // Curried or not?
    if body_node.kind == ASTKind::Lambda {
        collect_lambda_args(arena, body_node, args);
    }

    Some(())
}

/// Traverse the arena from a top-level SetEntry and collect, where
/// possible:
///
/// 1. The identifier of the set entry itself.
/// 2. The attached doc comment on the entry.
/// 3. The argument names of any curried functions (pattern functions
///    not yet supported).
fn collect_entry_information<'a>(arena: &Arena<'a>, entry_node: &ASTNode) -> Option<DocItem> {
    // The "root" of any attribute set entry is this `SetEntry` node.
    // It has an `Attribute` child, which in turn has the identifier
    // (on which the documentation comment is stored) as its child.
    let attr_node = &arena[entry_node.node.child?];
    let ident_node = &arena[attr_node.node.child?];

    // At this point we can retrieve the `DocItem` from the identifier
    // node - this already contains most of the information we are
    // interested in.
    let doc_item = retrieve_doc_item(ident_node)?;

    // From our entry we can walk two nodes to the right and check
    // whether we are dealing with a lambda. If so, we can start
    // collecting the function arguments - otherwise we're done.
    let assign_node = &arena[attr_node.node.sibling?];
    let content_node = &arena[assign_node.node.sibling?];

    if content_node.kind == ASTKind::Lambda {
        let mut args: Vec<String> = vec![];
        collect_lambda_args(arena, content_node, &mut args);
        Some(DocItem { args, ..doc_item })
    } else {
        Some(doc_item)
    }
}

fn main() {
    let opts = Options::from_args();
    let src = fs::read_to_string(&opts.file).unwrap();
    let nix = rnix::parse(&src).unwrap();

    let entries: Vec<ManualEntry> = nix.arena.into_iter()
        .filter(|node| node.kind == ASTKind::SetEntry)
        .filter_map(|node| collect_entry_information(&nix.arena, node))
        .map(|d| ManualEntry {
            category: opts.category.clone(),
            name: d.name,
            description: d.comment.doc
                .split("\n\n")
                .map(|s| s.to_string())
                .collect(),
            fn_type: d.comment.doc_type,
            example: d.comment.example,
            args: d.args,
        })
        .collect();

    let mut writer = EmitterConfig::new()
        .perform_indent(true)
        .create_writer(io::stdout());

    writer.write(
        XmlEvent::start_element("section")
            .attr("xmlns", "http://docbook.org/ns/docbook")
            .attr("xmlns:xlink", "http://www.w3.org/1999/xlink")
            .attr("xmlns:xi", "http://www.w3.org/2001/XInclude")
            .attr("xml:id", format!("sec-functions-library-{}", opts.category).as_str()))
        .unwrap();

    writer.write(XmlEvent::start_element("title")).unwrap();
    writer.write(XmlEvent::characters(&opts.description)).unwrap();
    writer.write(XmlEvent::end_element()).unwrap();

    for entry in entries {
        entry.write_section_xml(&mut writer).expect("Failed to write section")
    }

    writer.write(XmlEvent::end_element()).unwrap();
}
