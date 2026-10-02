//! Draws a Replay domain as an event-storming map: aggregates with their commands and events, and the policies that
//! turn an aggregate's event into commands for aggregates. `cargo replay-map` is the command-line front end.
//!
//! It reads the domain's *source* with `syn` rather than anything `define_aggregate!` emits. The macro expands to
//! enums that know nothing about which command yields which event: that link exists only in the match arms of
//! `Aggregate::handle`, `AggregatePolicy::react` and `Policy::react`, which no macro sees. Reading source also needs
//! no build, no runtime and no database.
//!
//! That puts one structural rule on a domain that adopts the map: every `handle` and `react` arm builds its events or
//! commands **inline**, in result position, never through a helper call — `<Agg>Event::<Variant>` in `handle`,
//! `(urn, <Target>Command::<Variant>)` in `AggregatePolicy::react`, and
//! `Dispatch::to::<Agg>(urn, <Agg>Command::<Variant>)` in `Policy::react`. A policy reading several aggregates' events
//! through a `query_events!` wrapper names each as `<Wrapper>::<Agg>Event(<Agg>Event::<Variant>)`. An arm the
//! generator cannot read fails with `file:line`; nothing is skipped.
//!
//! ```no_run
//! use replay_map::{DomainMap, read_sources};
//!
//! let sources = read_sources(std::path::Path::new("src"))?;
//! let markdown = DomainMap::read(&sources)?.to_markdown();
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::fs;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, Span, TokenTree};
use quote::ToTokens;
use syn::parse::{ParseStream, Parser};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    braced, bracketed, Attribute, Block, Expr, ExprClosure, ExprMatch, ExprReturn, GenericArgument,
    Generics, Ident, ImplItem, Item, ItemImpl, Macro, Meta, Pat, PathArguments, Stmt, Token, Type,
    Variant,
};

/// A domain construct the generator cannot read, at the place it met it.
#[derive(Debug)]
pub struct MapError {
    pub file: String,
    pub line: usize,
    pub message: String,
}

impl std::error::Error for MapError {}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}: {}", self.file, self.line, self.message)
    }
}

type Result<T> = std::result::Result<T, MapError>;

/// One source file of the domain, named by the path errors should print.
pub struct Source {
    pub path: String,
    pub text: String,
}

/// Every `.rs` file under `dir`, in path order so the map does not depend on the file system's.
pub fn read_sources(dir: &Path) -> std::io::Result<Vec<Source>> {
    let mut out = Vec::new();
    collect(dir, &mut out)?;
    Ok(out)
}

fn collect(dir: &Path, out: &mut Vec<Source>) -> std::io::Result<()> {
    let mut paths = fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<std::io::Result<Vec<PathBuf>>>()?;
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(Source {
                text: fs::read_to_string(&path)?,
                path: path.display().to_string(),
            });
        }
    }
    Ok(())
}

/// `(aggregate, variant)`: a command or event, named with the aggregate that declares it.
pub type Ref = (String, String);

#[derive(Debug, PartialEq)]
pub struct Aggregate {
    pub name: String,
    pub commands: Vec<String>,
    pub events: Vec<String>,
    /// `(command, event)`: the command's `handle` arm builds the event.
    pub decisions: Vec<(String, String)>,
}

#[derive(Debug, PartialEq)]
pub struct Policy {
    pub name: String,
    /// One per command-issuing `react` arm: the events it matches, and the commands it issues.
    pub reactions: Vec<(Vec<Ref>, Vec<Ref>)>,
}

#[derive(Debug)]
pub struct DomainMap {
    pub aggregates: Vec<Aggregate>,
    pub policies: Vec<Policy>,
}

impl DomainMap {
    pub fn read(sources: &[Source]) -> Result<Self> {
        let mut files = Vec::new();
        for source in sources {
            let file = syn::parse_file(&source.text).map_err(|e| {
                error(
                    &source.path,
                    e.span(),
                    format!("expected Rust that parses: {e}"),
                )
            })?;
            files.push((source.path.as_str(), file));
        }
        // Declarations first: a policy's arms can only be read once every `query_events!` wrapper is known.
        let mut scan = Scan::default();
        for (path, file) in &files {
            walk(&file.items, &mut |item| scan.declaration(path, item))?;
        }
        for (path, file) in &files {
            walk(&file.items, &mut |item| scan.behaviour(path, item))?;
        }
        scan.assemble()
    }

    pub fn to_markdown(&self) -> String {
        let mut out = String::from(concat!(
            "# Domain map\n\n",
            "<!-- Generated by `cargo replay-map`. Do not edit by hand. -->\n\n",
            "Aggregates are yellow, commands blue, events orange and policies purple.\n\n",
            "```mermaid\n",
            // Text is pinned to black so no renderer theme can grey it out, and every fill keeps it at WCAG AAA (7:1 or
            // better). The mid-grey lines stay above 3:1 on a light and a dark page, since the renderer's theme is the
            // reader's.
            "---\n",
            "config:\n",
            "  theme: base\n",
            "  themeVariables:\n",
            "    lineColor: \"#808080\"\n",
            "    primaryTextColor: \"#000000\"\n",
            "    nodeTextColor: \"#000000\"\n",
            "    titleColor: \"#000000\"\n",
            "---\n",
            "flowchart LR\n",
            "    classDef command fill:#9dc3e3,stroke:#4a7599,color:#000\n",
            "    classDef event fill:#eeb07c,stroke:#9c6232,color:#000\n",
            "    classDef policy fill:#c7b3de,stroke:#6f5790,color:#000\n",
        ));
        // Writing to a `String` cannot fail.
        for aggregate in &self.aggregates {
            let name = &aggregate.name;
            let group = node_id("agg", &[name]);
            let _ = writeln!(out, "\n    subgraph {group} [{name}]");
            for command in &aggregate.commands {
                let _ = writeln!(
                    out,
                    "        {}(\"{command}\"):::command",
                    command_id(name, command)
                );
            }
            for event in &aggregate.events {
                let _ = writeln!(
                    out,
                    "        {}(\"{event}\"):::event",
                    event_id(name, event)
                );
            }
            let _ = writeln!(out, "    end");
            let _ = writeln!(
                out,
                "    style {group} fill:#eedc92,stroke:#8f7d33,color:#000"
            );
            for (command, event) in &aggregate.decisions {
                let _ = writeln!(
                    out,
                    "    {} --> {}",
                    command_id(name, command),
                    event_id(name, event)
                );
            }
        }
        for policy in &self.policies {
            for (index, (events, commands)) in policy.reactions.iter().enumerate() {
                let node = node_id("pol", &[&policy.name, &index.to_string()]);
                let _ = writeln!(out, "\n    {node}(\"{}\"):::policy", policy.name);
                for (aggregate, event) in events {
                    let _ = writeln!(out, "    {} --> {node}", event_id(aggregate, event));
                }
                for (aggregate, command) in commands {
                    let _ = writeln!(out, "    {node} --> {}", command_id(aggregate, command));
                }
            }
        }
        out.push_str("```\n");
        out
    }
}

fn command_id(aggregate: &str, command: &str) -> String {
    node_id("cmd", &[aggregate, command])
}

fn event_id(aggregate: &str, event: &str) -> String {
    node_id("evt", &[aggregate, event])
}

/// Joined with `-`, which no Rust identifier contains, so two different name pairs never share a node: `_` would
/// make aggregate `A_B` with command `C` collide with aggregate `A` and command `B_C`. A raw identifier drops its
/// `r#`, which Mermaid cannot read.
fn node_id(kind: &str, names: &[&str]) -> String {
    std::iter::once(kind)
        .chain(names.iter().map(|name| name.trim_start_matches("r#")))
        .collect::<Vec<_>>()
        .join("-")
}

struct Located<T> {
    file: String,
    line: usize,
    value: T,
}

impl<T> Located<T> {
    fn at(file: &str, span: Span, value: T) -> Self {
        Self {
            file: file.to_owned(),
            line: span.start().line,
            value,
        }
    }

    fn error(&self, message: String) -> MapError {
        MapError {
            file: self.file.clone(),
            line: self.line,
            message,
        }
    }
}

struct AggregateDef {
    name: String,
    commands: Vec<String>,
    events: Vec<String>,
}

/// One match arm: what its pattern matches, and what it builds.
type Arm = Located<(Vec<Ref>, Vec<Ref>)>;

struct Handle {
    aggregate: String,
    arms: Vec<Arm>,
}

struct PolicyDef {
    name: String,
    arms: Vec<Arm>,
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Command,
    Event,
}

impl Kind {
    fn suffix(self) -> &'static str {
        match self {
            Kind::Command => "Command",
            Kind::Event => "Event",
        }
    }
}

/// What a `match` in `handle` or `react` is over.
enum Matched {
    /// One aggregate's own command or event enum.
    Enum { aggregate: String, kind: Kind },
    /// A `query_events!` wrapper, whose variants are named after the event types it merges.
    Wrapper { name: String, members: Vec<String> },
}

/// What a `handle` or `react` arm returns.
enum Builds {
    /// `<Agg>Event::<Variant>` values, from `handle`.
    Events(String),
    /// `(urn, <Target>Command::<Variant>)` pairs, from `AggregatePolicy::react`.
    Pairs(String),
    /// `Dispatch::to::<Agg>(urn, <Agg>Command::<Variant>)`, from `Policy::react`.
    Dispatches,
}

#[derive(Default)]
struct Scan {
    aggregates: Vec<Located<AggregateDef>>,
    /// `query_events!` wrapper name → the event types it merges.
    wrappers: BTreeMap<String, Vec<String>>,
    handles: Vec<Located<Handle>>,
    policies: Vec<Located<PolicyDef>>,
}

/// Every item, descending into inline modules, except those compiled only for tests.
fn walk<'a>(items: &'a [Item], visit: &mut impl FnMut(&'a Item) -> Result<()>) -> Result<()> {
    for item in items {
        let attrs = match item {
            Item::Macro(m) => &m.attrs,
            Item::Impl(i) => &i.attrs,
            Item::Mod(m) => &m.attrs,
            _ => continue,
        };
        if is_test(attrs) {
            continue;
        }
        match item {
            Item::Mod(m) => {
                if let Some((_, items)) = &m.content {
                    walk(items, visit)?;
                }
            }
            _ => visit(item)?,
        }
    }
    Ok(())
}

impl Scan {
    fn declaration(&mut self, file: &str, item: &Item) -> Result<()> {
        let Item::Macro(m) = item else { return Ok(()) };
        if is_named(&m.mac.path, "define_aggregate") {
            self.aggregates.push(aggregate(file, &m.mac)?);
        } else if is_named(&m.mac.path, "query_events") {
            let (name, members) = wrapper(file, &m.mac)?;
            if self.wrappers.insert(name.clone(), members).is_some() {
                return Err(error(
                    file,
                    m.mac.path.span(),
                    format!("one `query_events!` named `{name}`"),
                ));
            }
        }
        Ok(())
    }

    fn behaviour(&mut self, file: &str, item: &Item) -> Result<()> {
        let Item::Impl(imp) = item else { return Ok(()) };
        match imp
            .trait_
            .as_ref()
            .and_then(|(_, path, _)| path.segments.last())
        {
            Some(t) if t.ident == "Aggregate" => self.handles.push(handle(file, imp)?),
            Some(t) if t.ident == "AggregatePolicy" => {
                let policy = policy(file, imp, &self.wrappers, true)?;
                self.policies.push(policy);
            }
            Some(t) if t.ident == "Policy" => {
                let policy = policy(file, imp, &self.wrappers, false)?;
                self.policies.push(policy);
            }
            _ => {}
        }
        Ok(())
    }

    fn assemble(self) -> Result<DomainMap> {
        let mut aggregates: BTreeMap<String, (Located<AggregateDef>, Option<Vec<Arm>>)> =
            BTreeMap::new();
        for def in self.aggregates {
            if aggregates.contains_key(&def.value.name) {
                return Err(def.error(format!(
                    "one `define_aggregate!` named `{}`",
                    def.value.name
                )));
            }
            aggregates.insert(def.value.name.clone(), (def, None));
        }

        for handle in self.handles {
            let Some((_, arms)) = aggregates.get_mut(&handle.value.aggregate) else {
                return Err(handle.error(format!(
                    "`impl Aggregate for {}` to name a type declared with `define_aggregate!`",
                    handle.value.aggregate
                )));
            };
            if arms.is_some() {
                return Err(handle.error(format!(
                    "one `impl Aggregate for {}`",
                    handle.value.aggregate
                )));
            }
            *arms = Some(handle.value.arms);
        }

        let declared = |arm: &Arm, refs: &[Ref], kind: Kind| -> Result<()> {
            for (aggregate, variant) in refs {
                let def = aggregates.get(aggregate).map(|(def, _)| &def.value);
                let variants = def.map(|def| match kind {
                    Kind::Command => &def.commands,
                    Kind::Event => &def.events,
                });
                if !variants.is_some_and(|variants| variants.contains(variant)) {
                    return Err(arm.error(format!(
                        "`{aggregate}{}::{variant}` to be declared in `define_aggregate!`",
                        kind.suffix()
                    )));
                }
            }
            Ok(())
        };

        let mut map = DomainMap {
            aggregates: Vec::new(),
            policies: Vec::new(),
        };
        for (def, arms) in aggregates.values() {
            let Some(arms) = arms else {
                return Err(def.error(format!(
                    "an `impl Aggregate for {}` with a `handle`",
                    def.value.name
                )));
            };
            let mut decisions = Vec::new();
            for arm in arms {
                let (commands, events) = &arm.value;
                declared(arm, commands, Kind::Command)?;
                declared(arm, events, Kind::Event)?;
                for (_, command) in commands {
                    for (_, event) in events {
                        push_unique(&mut decisions, (command.clone(), event.clone()));
                    }
                }
            }
            map.aggregates.push(Aggregate {
                name: def.value.name.clone(),
                commands: def.value.commands.clone(),
                events: def.value.events.clone(),
                decisions,
            });
        }

        let mut policies = self.policies;
        policies.sort_by(|a, b| a.value.name.cmp(&b.value.name));
        if let Some(pair) = policies
            .windows(2)
            .find(|pair| pair[0].value.name == pair[1].value.name)
        {
            return Err(pair[1].error(format!("one policy named `{}`", pair[1].value.name)));
        }
        for policy in policies {
            let mut reactions = Vec::new();
            for arm in &policy.value.arms {
                let (events, commands) = &arm.value;
                declared(arm, events, Kind::Event)?;
                declared(arm, commands, Kind::Command)?;
                if !commands.is_empty() {
                    reactions.push(arm.value.clone());
                }
            }
            map.policies.push(Policy {
                name: policy.value.name.clone(),
                reactions,
            });
        }
        Ok(map)
    }
}

fn push_unique<T: PartialEq>(items: &mut Vec<T>, item: T) {
    if !items.contains(&item) {
        items.push(item);
    }
}

fn error(file: &str, span: Span, message: String) -> MapError {
    MapError {
        file: file.to_owned(),
        line: span.start().line,
        message,
    }
}

/// `#[cfg(test)]`, or any `cfg` that holds only under test, such as `#[cfg(all(test, not(target_arch = "wasm32")))]`.
fn is_test(attrs: &[Attribute]) -> bool {
    fn test_only(meta: &Meta) -> bool {
        match meta {
            Meta::Path(path) => path.is_ident("test"),
            Meta::List(list) if list.path.is_ident("all") => list
                .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                .is_ok_and(|all| all.iter().any(test_only)),
            _ => false,
        }
    }
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<Meta>().is_ok_and(|meta| test_only(&meta))
    })
}

fn is_named(path: &syn::Path, name: &str) -> bool {
    path.segments
        .last()
        .is_some_and(|segment| segment.ident == name)
}

fn snippet(tokens: &impl ToTokens) -> String {
    let text = tokens.to_token_stream().to_string();
    match text.char_indices().nth(80) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text,
    }
}

/// `Name<Generics> { key: value, .. }`, where `commands` and `events` are braced lists of enum variants and every other
/// value (`namespace`, `state`, `service: FileService + LogService { .. }`) runs to the next top-level comma and is
/// ignored, as are the generics.
fn aggregate(file: &str, mac: &Macro) -> Result<Located<AggregateDef>> {
    let parser = |input: ParseStream| -> syn::Result<AggregateDef> {
        let name: Ident = input.parse()?;
        input.parse::<Generics>()?;
        let body;
        braced!(body in input);
        let (mut commands, mut events) = (None, None);
        while !body.is_empty() {
            let key: Ident = body.parse()?;
            body.parse::<Token![:]>()?;
            let mut value = Vec::new();
            while !body.is_empty() && !body.peek(Token![,]) {
                value.push(body.parse::<TokenTree>()?);
            }
            let slot = match key.to_string().as_str() {
                "commands" => Some(&mut commands),
                "events" => Some(&mut events),
                _ => None,
            };
            if let Some(slot) = slot {
                let group = match value.as_slice() {
                    [TokenTree::Group(group)] if group.delimiter() == Delimiter::Brace => group,
                    _ => return Err(syn::Error::new(key.span(), format!("`{key}: {{ .. }}`"))),
                };
                let variants =
                    Punctuated::<Variant, Token![,]>::parse_terminated.parse2(group.stream())?;
                *slot = Some(
                    variants
                        .iter()
                        .map(|v| v.ident.to_string())
                        .collect::<Vec<_>>(),
                );
            }
            if !body.is_empty() {
                body.parse::<Token![,]>()?;
            }
        }
        let missing =
            |what: &str| syn::Error::new(name.span(), format!("`{what}: {{ .. }}` in `{name}`"));
        Ok(AggregateDef {
            commands: commands.ok_or_else(|| missing("commands"))?,
            events: events.ok_or_else(|| missing("events"))?,
            name: name.to_string(),
        })
    };
    let def = parser.parse2(mac.tokens.clone()).map_err(|e| {
        error(
            file,
            e.span(),
            format!("expected a `define_aggregate!` this generator can read: {e}"),
        )
    })?;
    Ok(Located::at(file, mac.path.span(), def))
}

/// `Name => [AEvent, BEvent]`.
fn wrapper(file: &str, mac: &Macro) -> Result<(String, Vec<String>)> {
    let parser = |input: ParseStream| -> syn::Result<(String, Vec<String>)> {
        let name: Ident = input.parse()?;
        input.parse::<Token![=>]>()?;
        let list;
        bracketed!(list in input);
        let members = Punctuated::<Type, Token![,]>::parse_terminated(&list)?;
        let members = members
            .iter()
            .map(|ty| type_name(ty).ok_or_else(|| syn::Error::new(ty.span(), "a named event type")))
            .collect::<syn::Result<_>>()?;
        Ok((name.to_string(), members))
    };
    parser.parse2(mac.tokens.clone()).map_err(|e| {
        error(
            file,
            e.span(),
            format!("expected a `query_events!` this generator can read: {e}"),
        )
    })
}

fn handle(file: &str, imp: &ItemImpl) -> Result<Located<Handle>> {
    let aggregate = self_name(file, imp)?;
    let matched = Matched::Enum {
        aggregate: aggregate.clone(),
        kind: Kind::Command,
    };
    let reader = Reader {
        file,
        builds: Builds::Events(aggregate.clone()),
    };
    let arms = arms(file, tail_match(file, imp, "handle")?, &matched, &reader)?;
    Ok(Located::at(
        file,
        imp.self_ty.span(),
        Handle { aggregate, arms },
    ))
}

/// An `impl AggregatePolicy` when `targeted`, otherwise a raw `impl Policy`.
fn policy(
    file: &str,
    imp: &ItemImpl,
    wrappers: &BTreeMap<String, Vec<String>>,
    targeted: bool,
) -> Result<Located<PolicyDef>> {
    let name = self_name(file, imp)?;
    let associated = |wanted: &str| {
        imp.items
            .iter()
            .find_map(|item| match item {
                ImplItem::Type(t) if t.ident == wanted => type_name(&t.ty),
                _ => None,
            })
            .ok_or_else(|| {
                error(
                    file,
                    imp.self_ty.span(),
                    format!("`type {wanted} = <Type>;` in the policy `{name}`"),
                )
            })
    };
    let event = associated("Event")?;
    let matched = match (wrappers.get(&event), event.strip_suffix("Event")) {
        (Some(members), _) => Matched::Wrapper {
            name: event.clone(),
            members: members.clone(),
        },
        (None, Some(aggregate)) => Matched::Enum {
            aggregate: aggregate.to_owned(),
            kind: Kind::Event,
        },
        (None, None) => {
            return Err(error(
                file,
                imp.self_ty.span(),
                format!(
                    "`type Event` of `{name}` to be an aggregate's `<Agg>Event` or a `query_events!` wrapper; found \
                     `{event}`"
                ),
            ));
        }
    };
    let builds = if targeted {
        Builds::Pairs(associated("Target")?)
    } else {
        Builds::Dispatches
    };
    let reader = Reader { file, builds };
    let arms = arms(file, tail_match(file, imp, "react")?, &matched, &reader)?;
    Ok(Located::at(
        file,
        imp.self_ty.span(),
        PolicyDef { name, arms },
    ))
}

fn self_name(file: &str, imp: &ItemImpl) -> Result<String> {
    type_name(&imp.self_ty).ok_or_else(|| {
        error(
            file,
            imp.self_ty.span(),
            format!("a named type; found `{}`", snippet(&imp.self_ty)),
        )
    })
}

fn type_name(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}

/// The `match` that `fn <name>` ends in.
fn tail_match<'a>(file: &str, imp: &'a ItemImpl, name: &str) -> Result<&'a ExprMatch> {
    let function = imp.items.iter().find_map(|item| match item {
        ImplItem::Fn(f) if f.sig.ident == name => Some(f),
        _ => None,
    });
    let Some(function) = function else {
        return Err(error(
            file,
            imp.self_ty.span(),
            format!("`fn {name}` in `impl ... for {}`", snippet(&imp.self_ty)),
        ));
    };
    // A `return` before the dispatch would decide outside any arm, where no command or event names it.
    let (tail, before) = function.block.stmts.split_last().unzip();
    let mut returns = Returns(Vec::new());
    before
        .into_iter()
        .flatten()
        .for_each(|stmt| returns.visit_stmt(stmt));
    if let Some(early) = returns.0.first() {
        return Err(error(
            file,
            early.span(),
            format!("`fn {name}` to return only from the arms of its closing `match`"),
        ));
    }
    match tail {
        Some(Stmt::Expr(Expr::Match(m), None)) => Ok(m),
        _ => Err(error(
            file,
            function.sig.ident.span(),
            format!("`fn {name}` to end in a `match` whose arms name one variant each"),
        )),
    }
}

fn arms(file: &str, body: &ExprMatch, matched: &Matched, reader: &Reader) -> Result<Vec<Arm>> {
    body.arms
        .iter()
        .map(|arm| {
            let mut patterns = Vec::new();
            let mut wildcard = false;
            variants(file, &arm.pat, matched, &mut patterns, &mut wildcard)?;
            let mut built = Vec::new();
            reader.expr(&arm.body, &mut built)?;
            // A wildcard names no variant, so there is nothing to draw an edge from.
            if wildcard && !built.is_empty() {
                return Err(error(
                    file,
                    arm.pat.span(),
                    "an arm that builds something to name the variants it matches, not `_`"
                        .to_owned(),
                ));
            }
            let mut unique = Vec::new();
            built
                .into_iter()
                .for_each(|built| push_unique(&mut unique, built));
            Ok(Located::at(file, arm.pat.span(), (patterns, unique)))
        })
        .collect()
}

fn variants(
    file: &str,
    pat: &Pat,
    matched: &Matched,
    out: &mut Vec<Ref>,
    wildcard: &mut bool,
) -> Result<()> {
    let path = match pat {
        Pat::Or(or) => {
            return or
                .cases
                .iter()
                .try_for_each(|case| variants(file, case, matched, out, wildcard));
        }
        Pat::Paren(p) => return variants(file, &p.pat, matched, out, wildcard),
        Pat::Reference(r) => return variants(file, &r.pat, matched, out, wildcard),
        Pat::Wild(_) => {
            *wildcard = true;
            return Ok(());
        }
        Pat::Path(p) => Some(&p.path),
        Pat::Struct(s) => Some(&s.path),
        Pat::TupleStruct(t) => Some(&t.path),
        _ => None,
    };
    let found = match matched {
        Matched::Enum { aggregate, kind } => path
            .and_then(|path| variant_of(path, &format!("{aggregate}{}", kind.suffix())))
            .map(|variant| out.push((aggregate.clone(), variant))),
        Matched::Wrapper { name, members } => {
            match (pat, path.and_then(|path| variant_of(path, name))) {
                (Pat::TupleStruct(t), Some(member))
                    if t.elems.len() == 1 && members.contains(&member) =>
                {
                    let inner = Matched::Enum {
                        aggregate: member.trim_end_matches("Event").to_owned(),
                        kind: Kind::Event,
                    };
                    return variants(file, &t.elems[0], &inner, out, wildcard);
                }
                _ => None,
            }
        }
    };
    found.ok_or_else(|| {
        let expected = match matched {
            Matched::Enum { aggregate, kind } => {
                format!("`{aggregate}{}::<Variant>`", kind.suffix())
            }
            Matched::Wrapper { name, .. } => format!("`{name}::<Agg>Event(<Agg>Event::<Variant>)`"),
        };
        error(
            file,
            pat.span(),
            format!("a match arm pattern {expected}; found `{}`", snippet(pat)),
        )
    })
}

fn variant_of(path: &syn::Path, prefix: &str) -> Option<String> {
    let segments: Vec<_> = path.segments.iter().collect();
    match segments.as_slice() {
        [head, variant] if head.ident == prefix && head.arguments.is_none() => {
            Some(variant.ident.to_string())
        }
        _ => None,
    }
}

fn path_of(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Path(p) => Some(
            p.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect(),
        ),
        _ => None,
    }
}

/// The aggregate named by `Dispatch::to::<Agg>`.
fn dispatch_target(func: &Expr) -> Option<String> {
    let Expr::Path(p) = func else { return None };
    let segments: Vec<_> = p.path.segments.iter().collect();
    let [dispatch, to] = segments.as_slice() else {
        return None;
    };
    if dispatch.ident != "Dispatch" || to.ident != "to" {
        return None;
    }
    let PathArguments::AngleBracketed(generics) = &to.arguments else {
        return None;
    };
    match generics.args.iter().collect::<Vec<_>>().as_slice() {
        [GenericArgument::Type(ty)] => type_name(ty),
        _ => None,
    }
}

/// Reads what one arm builds, following every expression in result position down to its constructors.
struct Reader<'a> {
    file: &'a str,
    builds: Builds,
}

impl Reader<'_> {
    fn expr(&self, expr: &Expr, out: &mut Vec<Ref>) -> Result<()> {
        match expr {
            Expr::Paren(p) => self.expr(&p.expr, out),
            Expr::Group(g) => self.expr(&g.expr, out),
            Expr::Block(b) => self.block(&b.block, out),
            Expr::If(i) => {
                self.block(&i.then_branch, out)?;
                match &i.else_branch {
                    Some((_, otherwise)) => self.expr(otherwise, out),
                    None => Ok(()),
                }
            }
            Expr::Match(m) => m.arms.iter().try_for_each(|arm| self.expr(&arm.body, out)),
            Expr::Return(r) => self.returned(r, out),
            Expr::Call(call) => match (path_of(&call.func).as_deref(), &self.builds) {
                (Some([err]), _) if err == "Err" => Ok(()),
                (Some([wrap]), _) if (wrap == "Ok" || wrap == "Some") && call.args.len() == 1 => {
                    self.expr(&call.args[0], out)
                }
                (Some([vec, new]), _) if vec == "Vec" && new == "new" && call.args.is_empty() => {
                    Ok(())
                }
                (_, Builds::Dispatches) => match dispatch_target(&call.func) {
                    Some(target) if call.args.len() == 2 => {
                        self.constructor(&call.args[1], &target, Kind::Command, out)
                    }
                    _ => Err(self.unreadable(expr)),
                },
                (_, Builds::Events(aggregate)) => {
                    self.constructor(expr, aggregate, Kind::Event, out)
                }
                (_, Builds::Pairs(_)) => Err(self.unreadable(expr)),
            },
            Expr::Path(p) if p.path.is_ident("None") => Ok(()),
            Expr::Macro(m) if m.mac.path.is_ident("vec") => {
                let items = Punctuated::<Expr, Token![,]>::parse_terminated
                    .parse2(m.mac.tokens.clone())
                    .map_err(|_| self.unreadable(expr))?;
                items.iter().try_for_each(|item| self.expr(item, out))
            }
            Expr::MethodCall(call) => match call.method.to_string().as_str() {
                "collect" | "unwrap_or_default" if call.args.is_empty() => {
                    self.expr(&call.receiver, out)
                }
                "with_metadata" if matches!(self.builds, Builds::Dispatches) => {
                    self.expr(&call.receiver, out)
                }
                "map" | "flat_map" | "filter_map" | "and_then" => match call.args.first() {
                    Some(Expr::Closure(closure))
                        if call.args.len() == 1 && !self.mentioned_in(&call.receiver) =>
                    {
                        self.expr(&closure.body, out)
                    }
                    _ => Err(self.unreadable(expr)),
                },
                _ => Err(self.unreadable(expr)),
            },
            Expr::Tuple(t) if t.elems.len() == 2 => match &self.builds {
                Builds::Pairs(target) => self.constructor(&t.elems[1], target, Kind::Command, out),
                _ => Err(self.unreadable(expr)),
            },
            _ => match &self.builds {
                Builds::Events(aggregate) => self.constructor(expr, aggregate, Kind::Event, out),
                _ => Err(self.unreadable(expr)),
            },
        }
    }

    /// Every `return` in the block, then its tail. A `return` inside a nested block is read twice, once from here
    /// and once from that block; `arms` dedupes what is built.
    fn block(&self, block: &Block, out: &mut Vec<Ref>) -> Result<()> {
        let mut returns = Returns(Vec::new());
        returns.visit_block(block);
        for r in returns.0 {
            self.returned(r, out)?;
        }
        match block.stmts.last() {
            Some(Stmt::Expr(tail, None)) => self.expr(tail, out),
            _ => Ok(()),
        }
    }

    fn returned(&self, r: &ExprReturn, out: &mut Vec<Ref>) -> Result<()> {
        match &r.expr {
            Some(value) => self.expr(value, out),
            None => Err(error(self.file, r.span(), self.expected("a bare `return`"))),
        }
    }

    fn constructor(
        &self,
        expr: &Expr,
        aggregate: &str,
        kind: Kind,
        out: &mut Vec<Ref>,
    ) -> Result<()> {
        let path = match expr {
            Expr::Path(p) => Some(&p.path),
            Expr::Struct(s) => Some(&s.path),
            Expr::Call(call) => match call.func.as_ref() {
                Expr::Path(p) => Some(&p.path),
                _ => None,
            },
            _ => None,
        };
        match path.and_then(|path| variant_of(path, &format!("{aggregate}{}", kind.suffix()))) {
            Some(variant) => {
                out.push((aggregate.to_owned(), variant));
                Ok(())
            }
            None => Err(self.unreadable(expr)),
        }
    }

    /// Whether `expr` builds anything itself, which a `.map` receiver must not: only the closure is read.
    fn mentioned_in(&self, expr: &Expr) -> bool {
        struct Mentions<'p>(&'p str, bool);
        impl Visit<'_> for Mentions<'_> {
            fn visit_path(&mut self, path: &syn::Path) {
                self.1 |= path
                    .segments
                    .first()
                    .is_some_and(|segment| segment.ident == self.0);
                visit::visit_path(self, path);
            }
        }
        let marker = match &self.builds {
            Builds::Events(aggregate) => format!("{aggregate}Event"),
            Builds::Pairs(target) => format!("{target}Command"),
            Builds::Dispatches => "Dispatch".to_owned(),
        };
        let mut mentions = Mentions(&marker, false);
        mentions.visit_expr(expr);
        mentions.1
    }

    fn unreadable(&self, expr: &Expr) -> MapError {
        error(
            self.file,
            expr.span(),
            self.expected(&format!("`{}`", snippet(expr))),
        )
    }

    fn expected(&self, found: &str) -> String {
        match &self.builds {
            Builds::Events(aggregate) => format!(
                "each `handle` arm to build its events inline, as `Ok(vec![{aggregate}Event::<Variant>])`, \
                 `Vec::new()` or `Err(..)`; found {found}"
            ),
            Builds::Pairs(target) => format!(
                "each `react` arm to build its commands inline, as `vec![(urn, {target}Command::<Variant>)]`, a \
                 `.map` to such a pair, or `Vec::new()`; found {found}"
            ),
            Builds::Dispatches => format!(
                "each `react` arm to build its commands inline, as `vec![Dispatch::to::<Agg>(urn, \
                 <Agg>Command::<Variant>)]`, a `.map` to such a dispatch, or `Vec::new()`; found {found}"
            ),
        }
    }
}

/// The `return`s that leave the function being read: not those of a closure or a nested item.
struct Returns<'ast>(Vec<&'ast ExprReturn>);

impl<'ast> Visit<'ast> for Returns<'ast> {
    fn visit_expr_return(&mut self, r: &'ast ExprReturn) {
        self.0.push(r);
        visit::visit_expr_return(self, r);
    }

    fn visit_expr_closure(&mut self, _: &'ast ExprClosure) {}

    fn visit_expr_async(&mut self, _: &'ast syn::ExprAsync) {}

    fn visit_item(&mut self, _: &'ast Item) {}
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const DRAWER: &str = r#"
define_aggregate! {
    Drawer {
        namespace: "drawer",
        state: { locked: bool },
        commands: { Fit { cabinet: CabinetUrn }, Lock, Unlock, Store(ItemUrn) },
        events: { Fitted { cabinet: CabinetUrn }, Locked, Unlocked, ItemStored(ItemUrn) }
    }
}

impl Aggregate for Drawer {
    async fn handle(&self, command: Self::Command, _: &()) -> Result<Vec<Self::Event>, AppError> {
        match command {
            DrawerCommand::Fit { cabinet } => {
                self.require(&cabinet)?;
                if self.cabinet.is_some() {
                    return Ok(Vec::new());
                }
                Ok(vec![DrawerEvent::Fitted { cabinet }])
            }
            DrawerCommand::Lock => Ok(if self.locked { Vec::new() } else { vec![DrawerEvent::Locked] }),
            DrawerCommand::Unlock => Err(AppError::conflict("never")),
            DrawerCommand::Store(item) => Ok(vec![DrawerEvent::ItemStored(item)]),
        }
    }
}
"#;

    const CABINET: &str = r#"
define_aggregate! {
    Cabinet {
        namespace: "cabinet",
        commands: { Seal, Install { drawer: DrawerUrn } },
        events: { Sealed { drawers: Vec<DrawerUrn> }, Installed { drawer: DrawerUrn }, Relabelled }
    }
}

impl Aggregate for Cabinet {
    async fn handle(&self, command: Self::Command, _: &()) -> Result<Vec<Self::Event>, AppError> {
        match command {
            CabinetCommand::Seal => Ok(vec![CabinetEvent::Sealed { drawers: self.drawers.clone() }]),
            CabinetCommand::Install { drawer } => Ok(vec![CabinetEvent::Installed { drawer }]),
        }
    }
}
"#;

    const POLICY: &str = r#"
pub struct DrawerPolicy;

impl AggregatePolicy for DrawerPolicy {
    type Event = CabinetEvent;
    type Target = Drawer;

    fn react(&self, event: &ObservedEvent<Self::Event>) -> Vec<(DrawerUrn, DrawerCommand)> {
        match &event.data {
            CabinetEvent::Installed { drawer } => CabinetUrn::try_from(event.stream_id.clone())
                .map(|cabinet| vec![(drawer.clone(), DrawerCommand::Fit { cabinet })])
                .unwrap_or_default(),
            CabinetEvent::Sealed { drawers } => drawers.iter().map(|d| (d.clone(), DrawerCommand::Lock)).collect(),
            CabinetEvent::Relabelled => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    impl AggregatePolicy for Unreadable {
        fn react(&self) -> Vec<()> { helper() }
    }
}
"#;

    /// A raw `Policy` reading two aggregates' events through a `query_events!` wrapper and commanding both.
    const HOUSEKEEPING: &str = r#"
query_events!(Workshop => [CabinetEvent, DrawerEvent]);

pub struct Housekeeping;

impl Policy for Housekeeping {
    type Event = Workshop;

    fn react(&self, event: &ObservedEvent<Self::Event>) -> Vec<Dispatch> {
        match &event.data {
            Workshop::CabinetEvent(CabinetEvent::Sealed { drawers }) => drawers
                .iter()
                .map(|d| Dispatch::to::<Drawer>(d.clone(), DrawerCommand::Lock).with_metadata(trace()))
                .collect(),
            Workshop::DrawerEvent(DrawerEvent::Locked | DrawerEvent::Unlocked) => {
                vec![Dispatch::to::<Cabinet>(cabinet(), CabinetCommand::Seal)]
            }
            _ => Vec::new(),
        }
    }
}
"#;

    fn read_all(sources: &[(&str, &str)]) -> Result<DomainMap> {
        let sources: Vec<_> = sources
            .iter()
            .map(|(path, text)| Source {
                path: path.to_string(),
                text: text.to_string(),
            })
            .collect();
        DomainMap::read(&sources)
    }

    fn read(drawer: &str, policy: &str) -> Result<DomainMap> {
        read_all(&[
            ("cabinet.rs", CABINET),
            ("drawer.rs", drawer),
            ("policy.rs", policy),
        ])
    }

    fn line_of(text: &str, needle: &str) -> usize {
        text.lines().position(|line| line.contains(needle)).unwrap() + 1
    }

    fn refs(items: &[(&str, &str)]) -> Vec<Ref> {
        items
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn every_aggregate_lists_its_commands_and_events_by_name() {
        let map = read(DRAWER, POLICY).unwrap();

        let names: Vec<_> = map.aggregates.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["Cabinet", "Drawer"]);
        let drawer = &map.aggregates[1];
        assert_eq!(drawer.commands, ["Fit", "Lock", "Unlock", "Store"]);
        assert_eq!(
            drawer.events,
            ["Fitted", "Locked", "Unlocked", "ItemStored"]
        );
    }

    #[test]
    fn a_command_leads_to_every_event_its_arm_builds_and_to_nothing_else() {
        let map = read(DRAWER, POLICY).unwrap();

        assert_eq!(
            map.aggregates[1].decisions,
            refs(&[
                ("Fit", "Fitted"),
                ("Lock", "Locked"),
                ("Store", "ItemStored")
            ])
        );
    }

    #[test]
    fn a_reacting_arm_leads_from_its_event_to_the_command_it_issues() {
        let map = read(DRAWER, POLICY).unwrap();

        assert_eq!(
            map.policies[0].reactions,
            [
                (
                    refs(&[("Cabinet", "Installed")]),
                    refs(&[("Drawer", "Fit")])
                ),
                (refs(&[("Cabinet", "Sealed")]), refs(&[("Drawer", "Lock")])),
            ]
        );
    }

    #[test]
    fn a_raw_policy_leads_from_each_wrapped_event_to_the_commands_it_dispatches() {
        let map = read_all(&[
            ("cabinet.rs", CABINET),
            ("drawer.rs", DRAWER),
            ("housekeeping.rs", HOUSEKEEPING),
        ])
        .unwrap();

        let policy = &map.policies[0];
        assert_eq!(policy.name, "Housekeeping");
        assert_eq!(
            policy.reactions,
            [
                (refs(&[("Cabinet", "Sealed")]), refs(&[("Drawer", "Lock")])),
                (
                    refs(&[("Drawer", "Locked"), ("Drawer", "Unlocked")]),
                    refs(&[("Cabinet", "Seal")])
                ),
            ]
        );
    }

    #[test]
    fn a_wildcard_arm_that_dispatches_fails_at_its_line() {
        let policy = HOUSEKEEPING.replace(
            "_ => Vec::new(),",
            "_ => vec![Dispatch::to::<Cabinet>(cabinet(), CabinetCommand::Seal)],",
        );

        let error = read_all(&[
            ("cabinet.rs", CABINET),
            ("drawer.rs", DRAWER),
            ("p.rs", &policy),
        ])
        .unwrap_err();

        assert_eq!(error.line, line_of(&policy, "_ => vec!["));
    }

    #[test]
    fn a_handle_arm_that_builds_its_events_through_a_helper_fails_at_its_line() {
        let drawer = DRAWER.replace(
            "Ok(vec![DrawerEvent::ItemStored(item)])",
            "self.store(item)",
        );

        let error = read(&drawer, POLICY).unwrap_err();

        assert_eq!(
            (error.file.as_str(), error.line),
            ("drawer.rs", line_of(&drawer, "self.store(item)"))
        );
    }

    #[test]
    fn a_react_arm_that_builds_its_commands_through_a_helper_fails_at_its_line() {
        let policy = POLICY.replace(
            "drawers.iter().map(|d| (d.clone(), DrawerCommand::Lock)).collect()",
            "lock_all(drawers)",
        );

        let error = read(DRAWER, &policy).unwrap_err();

        assert_eq!(
            (error.file.as_str(), error.line),
            ("policy.rs", line_of(&policy, "lock_all(drawers)"))
        );
    }

    #[test]
    fn a_raw_policy_arm_that_dispatches_through_a_helper_fails_at_its_line() {
        let policy = HOUSEKEEPING.replace(
            "vec![Dispatch::to::<Cabinet>(cabinet(), CabinetCommand::Seal)]",
            "self.reseal()",
        );

        let error = read_all(&[
            ("cabinet.rs", CABINET),
            ("drawer.rs", DRAWER),
            ("p.rs", &policy),
        ])
        .unwrap_err();

        assert_eq!(
            (error.file.as_str(), error.line),
            ("p.rs", line_of(&policy, "self.reseal()"))
        );
    }

    #[test]
    fn an_arm_that_builds_an_undeclared_variant_fails_at_its_line() {
        let drawer = DRAWER.replace("vec![DrawerEvent::Locked]", "vec![DrawerEvent::Jammed]");

        let error = read(&drawer, POLICY).unwrap_err();

        assert_eq!(error.line, line_of(&drawer, "DrawerCommand::Lock =>"));
    }

    #[test]
    fn a_return_before_the_dispatch_fails_at_its_line() {
        let guard = "if self.sealed { return Ok(vec![DrawerEvent::Locked]); }";
        let drawer = DRAWER.replace(
            "        match command {",
            &format!("        {guard}\n        match command {{"),
        );

        let error = read(&drawer, POLICY).unwrap_err();

        assert_eq!(error.line, line_of(&drawer, guard));
    }

    #[test]
    fn two_policies_with_one_name_fail() {
        let policy = format!(
            "{POLICY}\nmod again {{\n{}\n}}",
            POLICY.split("#[cfg(test)]").next().unwrap()
        );

        assert!(read(DRAWER, &policy).is_err());
    }

    #[test]
    fn names_joined_by_an_underscore_still_get_distinct_nodes() {
        let source = |aggregate: &str, command: &str| {
            format!(
                "define_aggregate! {{ {aggregate} {{ commands: {{ {command} }}, events: {{ Done }} }} }}
                 impl Aggregate for {aggregate} {{
                     async fn handle(&self, c: Self::Command, _: &()) -> R {{
                         match c {{ {aggregate}Command::{command} => Ok(vec![{aggregate}Event::Done]) }}
                     }}
                 }}"
            )
        };
        let (a_b, a) = (source("A_B", "C"), source("A", "B_C"));

        let markdown = read_all(&[("a_b.rs", &a_b), ("a.rs", &a)])
            .unwrap()
            .to_markdown();

        assert!(markdown.contains("cmd-A_B-C("));
        assert!(markdown.contains("cmd-A-B_C("));
    }

    #[test]
    fn an_aggregate_with_generics_and_service_sections_is_read() {
        let generic = r#"
define_aggregate! {
    FileManager<T: PartialEq> {
        state: { processed: T },
        commands: { ProcessFile { data: T } },
        events: { FileProcessed { data: T } },
        service: FileService + LogService {
            fn validate(entry: &str) -> bool;
        }
    }
}

impl<T: PartialEq> Aggregate for FileManager<T> {
    async fn handle(&self, command: Self::Command, _: &Self::Services) -> R {
        match command {
            FileManagerCommand::ProcessFile { data } => Ok(vec![FileManagerEvent::FileProcessed { data }]),
        }
    }
}
"#;

        let map = read_all(&[("file_manager.rs", generic)]).unwrap();

        assert_eq!(
            map.aggregates[0].decisions,
            refs(&[("ProcessFile", "FileProcessed")])
        );
    }

    #[test]
    fn a_module_compiled_only_under_test_is_skipped_however_its_cfg_is_spelled() {
        let tests = r#"
#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    impl Aggregate for OnlyInTests {
        async fn handle(&self) -> R { helper() }
    }
}
"#;

        assert!(read_all(&[("cabinet.rs", CABINET), ("tests.rs", tests)]).is_ok());
    }

    #[test]
    fn the_map_draws_each_event_once_and_never_the_state() {
        let markdown = read(DRAWER, POLICY).unwrap().to_markdown();

        assert_eq!(markdown.matches("evt-Drawer-Locked(").count(), 1);
        assert!(markdown.contains("subgraph agg-Drawer [Drawer]"));
        assert!(markdown.contains("evt-Cabinet-Sealed --> pol-DrawerPolicy-1"));
        assert!(markdown.contains("pol-DrawerPolicy-1 --> cmd-Drawer-Lock"));
        assert!(!markdown.contains("bool"));
    }
}
