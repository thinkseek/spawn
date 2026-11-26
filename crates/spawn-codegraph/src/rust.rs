//! What one Rust file says, read with `syn` (`full`, `visit`): its module
//! declarations, items, `use` trees, paths (in code, types, patterns,
//! attributes and macro bodies) and `include*!` files, per module.
//!
//! Nothing here resolves a name: [`crate::resolve`] does that against the
//! module tree and the in-scope inventory.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use proc_macro2::{Delimiter, Spacing, Span, TokenStream, TokenTree};
use syn::visit::{self, Visit};
use syn::{Attribute, Item, UseTree};

/// The facts of one file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct FileFacts {
    pub root: ModuleFacts,
    /// Some part `syn` refused, or that could not be cut to
    /// [`MAX_SYN_TOKENS`]: its facts come from a token walk.
    pub token_fallback: bool,
}

/// The facts of one module (a file, or an inline `mod name { ... }`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ModuleFacts {
    /// Names of items declared directly in the module (fn, struct, enum,
    /// union, trait, const, static, type, `macro_rules!`).
    pub items: BTreeSet<String>,
    pub mods: Vec<ModDecl>,
    pub uses: Vec<UseEntry>,
    pub paths: Vec<PathRef>,
    pub includes: Vec<IncludeRef>,
    /// Single-segment names used in the module's code and types, each with
    /// its first line (a production use preferred) and whether that use is
    /// test-only: how a `pub use` name the module also uses itself keeps a
    /// real dependency edge besides its structural re-export.
    pub local_refs: BTreeMap<String, (u32, bool)>,
    /// Identifiers in the module's item-level macro invocations
    /// (`label!(SessionId, "session");`): the items such a macro may
    /// generate, which `syn` cannot see.
    pub macro_items: BTreeSet<String>,
}

impl ModuleFacts {
    /// The facts of the inline module at `inline` below this one.
    pub(crate) fn nested(&self, inline: &[String]) -> Option<&Self> {
        let Some((first, rest)) = inline.split_first() else {
            return Some(self);
        };
        self.mods
            .iter()
            .find(|decl| &decl.name == first && decl.inline.is_some())
            .and_then(|decl| decl.inline.as_ref())
            .and_then(|inner| inner.nested(rest))
    }
}

/// `mod name;` or `mod name { ... }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ModDecl {
    pub name: String,
    pub line: u32,
    /// Under `#[cfg(test)]`.
    pub test: bool,
    /// `#[path = "..."]`.
    pub path_attr: Option<String>,
    /// The body of an inline module.
    pub inline: Option<ModuleFacts>,
}

/// One leaf of an expanded `use` tree, or an `extern crate`.
// Four independent facts about one syntax leaf, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct UseEntry {
    pub segments: Vec<String>,
    /// The name it introduces (`as` alias, else the last segment); `None`
    /// for a glob or `as _`.
    pub name: Option<String>,
    pub glob: bool,
    /// Any visibility but private: others may name it through this module.
    pub public: bool,
    /// `use ::name` or `extern crate`: the first segment is a crate.
    pub leading_colon: bool,
    pub line: u32,
    pub test: bool,
}

/// A path of two or more segments (or with a leading `::`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PathRef {
    pub segments: Vec<String>,
    pub leading_colon: bool,
    pub line: u32,
    pub test: bool,
}

/// `include!`, `include_str!` or `include_bytes!` with a literal path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IncludeRef {
    pub path: String,
    /// `concat!(env!("CARGO_MANIFEST_DIR"), "...")`: relative to the
    /// package root rather than the file.
    pub from_manifest_dir: bool,
    pub line: u32,
    pub test: bool,
}

fn line_of(span: Span) -> u32 {
    u32::try_from(span.start().line).unwrap_or(u32::MAX)
}

/// Deepest bracket nesting (`(`, `[`, `{`) a file may have. A deeper file is
/// listed as unparsed, never lexed into a tree: every walk of the tree
/// (the reader below, `syn`'s token buffer, the walks over macro tokens)
/// goes at most one level deeper per bracket.
pub const MAX_NESTING: usize = 256;

/// The most tokens one `syn` call reads: every leaf and every group of the
/// tokens it is handed, a macro call's delimited body (`name!(…)`,
/// `macro_rules! name {…}`) counting as one, since `syn` keeps that body
/// unparsed. A recursive-descent parser nests at most a bounded number of
/// frames per token it consumes, and the visitor and the drop of what it
/// builds nest no deeper than that, so this bounds the stack every `syn`
/// call can take, whatever the input's shape. The costliest shapes measured
/// in a debug build take about 31 KiB of stack per token (`&&&T`, `[[T]]`,
/// `((T,),)`): about 31 MiB at this budget, half the parser thread's 64 MiB.
/// The reader below splits larger items, bodies and expressions so that no
/// call gets more; a piece it cannot split is read with a token walk.
pub(crate) const MAX_SYN_TOKENS: usize = 1_000;

/// The most tokens a wrapper adds around what the reader counted
/// (`impl H {…}`, `match H {…}`, `fn H() where … {}`, `let …;`): every
/// `syn` call reads at most `MAX_SYN_TOKENS + WRAPPER_TOKENS`.
const WRAPPER_TOKENS: usize = 8;

/// The text a file's tokens start at: after a byte-order mark and, as
/// `syn::parse_file` does, after a shebang line (`#!` not followed by `[`,
/// past spaces and comments, which would make it an inner attribute).
fn source_text(text: &str) -> &str {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some(rest) = text.strip_prefix("#!") {
        let bytes = rest.as_bytes();
        let mut at = 0;
        while at < bytes.len() {
            if bytes[at].is_ascii_whitespace() {
                at += 1;
            } else if starts_comment(bytes, at) {
                at = skip_comment(bytes, at);
            } else {
                break;
            }
        }
        if bytes.get(at) != Some(&b'[') {
            return text.find('\n').map_or("", |newline| &text[newline..]);
        }
    }
    text
}

/// The deepest bracket nesting of `text` outside comments, string and
/// char literals, in one iterative pass that stops past [`MAX_NESTING`].
/// A file that starts with `#!` is measured with and without its first
/// line, so no reading of that line can hide brackets after it.
fn bracket_depth(text: &str) -> usize {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let whole = bracket_depth_of(text);
    if text.starts_with("#!") {
        let rest = text.find('\n').map_or("", |newline| &text[newline..]);
        whole.max(bracket_depth_of(rest))
    } else {
        whole
    }
}

fn bracket_depth_of(text: &str) -> usize {
    let bytes = text.as_bytes();
    let (mut depth, mut deepest, mut at) = (0usize, 0usize, 0);
    while at < bytes.len() && deepest <= MAX_NESTING {
        let byte = bytes[at];
        if starts_comment(bytes, at) {
            at = skip_comment(bytes, at);
        } else if byte == b'"' {
            at = skip_quoted(bytes, at + 1, b'"');
        } else if byte == b'\'' {
            at = skip_char_or_lifetime(bytes, at).0;
        } else if is_ident_byte(byte) && (at == 0 || !is_ident_byte(bytes[at - 1])) {
            at = skip_prefixed_string(bytes, at).unwrap_or(at + 1);
        } else {
            match byte {
                b'(' | b'[' | b'{' => {
                    depth += 1;
                    deepest = deepest.max(depth);
                }
                b')' | b']' | b'}' => depth = depth.saturating_sub(1),
                _ => {}
            }
            at += 1;
        }
    }
    deepest
}

/// The deepest group nesting of lexed tokens, iteratively.
fn group_depth(tokens: &[TokenTree]) -> usize {
    let mut deepest = 0;
    let mut pending: Vec<(TokenStream, usize)> = tokens
        .iter()
        .filter_map(|token| match token {
            TokenTree::Group(group) => Some((group.stream(), 1)),
            _ => None,
        })
        .collect();
    while let Some((stream, depth)) = pending.pop() {
        deepest = deepest.max(depth);
        if depth > MAX_NESTING {
            break;
        }
        for token in stream {
            if let TokenTree::Group(group) = token {
                pending.push((group.stream(), depth + 1));
            }
        }
    }
    deepest
}

/// Words that never name a macro: Rust's keywords, reserved and contextual
/// ones. `name!(…)` is a macro call only when `name` is none of them (in
/// `if !(…)` the group is parsed).
const NOT_MACRO_NAMES: [&str; 54] = [
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type",
    "unsafe", "use", "where", "while", "abstract", "become", "box", "do", "final", "macro",
    "override", "priv", "typeof", "unsized", "virtual", "yield", "try", "gen", "union", "default",
];

fn punct_is(token: Option<&TokenTree>, ch: char) -> bool {
    matches!(token, Some(TokenTree::Punct(punct)) if punct.as_char() == ch)
}

fn ident_is(token: Option<&TokenTree>, word: &str) -> bool {
    matches!(token, Some(TokenTree::Ident(ident)) if ident == word)
}

/// Whether `tokens[at]`, a group, is a macro call's body: after `name !`
/// (`name` not a keyword) or `macro_rules ! name`.
fn macro_body_at(tokens: &[TokenTree], at: usize) -> bool {
    let before = |back: usize| at.checked_sub(back).and_then(|index| tokens.get(index));
    let named = |token: Option<&TokenTree>| {
        matches!(token, Some(TokenTree::Ident(ident))
            if !NOT_MACRO_NAMES.contains(&ident.to_string().as_str()))
    };
    // `'a !(…)`: a label or lifetime (`break 'a !(…)`, `&'a !`), no macro.
    let lifetime = |back: usize| punct_is(before(back), '\'');
    (punct_is(before(1), '!') && named(before(2)) && !lifetime(3))
        || (named(before(1))
            && punct_is(before(2), '!')
            && ident_is(before(3), "macro_rules")
            && !lifetime(4))
}

/// A group's place in its file: its span's start and end.
type SpanKey = (usize, usize, usize, usize);

fn span_key(span: Span) -> SpanKey {
    let (start, end) = (span.start(), span.end());
    (start.line, start.column, end.line, end.column)
}

/// `tokens` with every macro call's body emptied (its span kept), as
/// [`budget`] counts them; the bodies go to `bodies` by span. One level of
/// recursion per bracket: at most [`MAX_NESTING`].
fn strip_macro_bodies(
    tokens: &[TokenTree],
    bodies: &mut BTreeMap<SpanKey, TokenStream>,
    attribute: bool,
    in_attributes: &mut BTreeSet<SpanKey>,
) -> TokenStream {
    tokens
        .iter()
        .enumerate()
        .map(|(at, token)| match token {
            TokenTree::Group(inner) => {
                let before = |back: usize| at.checked_sub(back).and_then(|index| tokens.get(index));
                let attribute = attribute
                    || (inner.delimiter() == Delimiter::Bracket
                        && (punct_is(before(1), '#')
                            || (punct_is(before(1), '!') && punct_is(before(2), '#'))));
                let stream = if macro_body_at(tokens, at) {
                    bodies.insert(span_key(inner.span()), inner.stream());
                    if attribute {
                        in_attributes.insert(span_key(inner.span()));
                    }
                    TokenStream::new()
                } else {
                    let content: Vec<TokenTree> = inner.stream().into_iter().collect();
                    strip_macro_bodies(&content, bodies, attribute, in_attributes)
                };
                let mut emptied = proc_macro2::Group::new(inner.delimiter(), stream);
                emptied.set_span(inner.span());
                TokenTree::Group(emptied)
            }
            other => other.clone(),
        })
        .collect()
}

/// Every leaf and every group of `stream`, all of it read by `syn`.
/// Iterative.
fn token_count(stream: &TokenStream) -> usize {
    let mut count = 0;
    let mut pending = vec![stream.clone()];
    while let Some(stream) = pending.pop() {
        for token in stream {
            count += 1;
            if let TokenTree::Group(group) = token {
                pending.push(group.stream());
            }
        }
    }
    count
}

/// The tokens `syn` would read in `tokens`: each leaf and each group, a
/// macro call's body counting once. Iterative.
fn budget(tokens: &[TokenTree]) -> usize {
    let mut count = 0;
    let mut pending: Vec<TokenStream> = Vec::new();
    let mut tally = |tokens: &[TokenTree], pending: &mut Vec<TokenStream>| {
        for (at, token) in tokens.iter().enumerate() {
            count += 1;
            if let TokenTree::Group(group) = token
                && !macro_body_at(tokens, at)
            {
                pending.push(group.stream());
            }
        }
    };
    tally(tokens, &mut pending);
    while let Some(stream) = pending.pop() {
        let inner: Vec<TokenTree> = stream.into_iter().collect();
        tally(&inner, &mut pending);
    }
    count
}

/// What a run of tokens is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Body {
    /// A file's or inline module's items.
    Module,
    /// An `impl` block's items.
    Impl,
    /// A trait's items.
    Trait,
    /// A block's statements.
    Block,
    /// A `match`'s arms.
    Arms,
    /// A struct literal's fields.
    Fields,
    /// A struct or union definition's fields.
    FieldDefs,
    /// An enum's variants.
    Variants,
    /// Comma-separated expressions (call arguments, array elements).
    Elements,
    /// A `where` clause's predicates.
    Predicates,
    /// An `extern "…" {…}` block's foreign items.
    Foreign,
}

/// The kind of item a run of tokens declares, from its first keyword after
/// attributes, visibility and qualifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Mod,
    Impl,
    Trait,
    Fn,
    Struct,
    Enum,
    Union,
    /// An `extern "…" {…}` block.
    Foreign,
    Other,
}

fn item_kind(tokens: &[TokenTree]) -> Kind {
    let mut external = false;
    for token in tokens {
        match token {
            TokenTree::Punct(punct) if matches!(punct.as_char(), '#' | '!') => {}
            TokenTree::Group(group) if group.delimiter() != Delimiter::Brace => {}
            TokenTree::Literal(_) => {}
            TokenTree::Ident(ident) => match ident.to_string().as_str() {
                "pub" | "unsafe" | "async" | "const" | "default" | "safe" => {}
                "extern" => external = true,
                "mod" => return Kind::Mod,
                "impl" => return Kind::Impl,
                "trait" => return Kind::Trait,
                "fn" => return Kind::Fn,
                "struct" => return Kind::Struct,
                "enum" => return Kind::Enum,
                "union" => return Kind::Union,
                _ => return Kind::Other,
            },
            _ => break,
        }
    }
    if external
        && tokens.iter().all(|token| match token {
            TokenTree::Group(group) => group.delimiter() != Delimiter::Brace,
            _ => true,
        })
    {
        Kind::Foreign
    } else {
        Kind::Other
    }
}

/// Whether a `{…}` group followed by `next` is still inside its statement
/// or item: `else`, `as`, or an operator (`.`, `?`, `=`, `+`, …) follows.
fn continues_after_brace(next: &TokenTree) -> bool {
    match next {
        TokenTree::Ident(ident) => ident == "else" || ident == "as",
        TokenTree::Punct(punct) => ".?+-*/%^&|=<>!:".contains(punct.as_char()),
        _ => false,
    }
}

/// The units of a body: inner attributes, statements or items (each to
/// its `;` or to a `{…}` that ends it), arms, fields, variants, elements or
/// predicates (each to its `,`; in field definitions, variants and `where`
/// predicates, past the `,` inside `<…>`).
fn split(tokens: &[TokenTree], body: Body) -> Vec<&[TokenTree]> {
    let by_comma = matches!(
        body,
        Body::Arms
            | Body::Fields
            | Body::FieldDefs
            | Body::Variants
            | Body::Elements
            | Body::Predicates
    );
    // Types: a `,` inside `<…>` separates nothing.
    let typed = matches!(body, Body::FieldDefs | Body::Variants | Body::Predicates);
    let mut units = Vec::new();
    let (mut start, mut at, mut angles) = (0, 0, 0usize);
    // The kind of item the unit from `start` declares, read once: a later
    // `{…}` in the unit cannot change it.
    let mut kind: Option<Kind> = None;
    while at < tokens.len() {
        let mut end = None;
        // An inner attribute (`#![doc = "…"]`, a `//!` line) is a unit of
        // its own: a file's may run to thousands of tokens.
        if at == start
            && punct_is(tokens.get(at), '#')
            && punct_is(tokens.get(at + 1), '!')
            && matches!(tokens.get(at + 2), Some(TokenTree::Group(inner)) if inner.delimiter() == Delimiter::Bracket)
        {
            units.push(&tokens[at..at + 3]);
            at += 3;
            start = at;
            continue;
        }
        match &tokens[at] {
            TokenTree::Punct(punct) if punct.as_char() == ';' && !by_comma => end = Some(at),
            TokenTree::Punct(punct) if punct.as_char() == ',' && by_comma && angles == 0 => {
                end = Some(at);
            }
            TokenTree::Punct(punct) if typed && punct.as_char() == '<' => {
                angles += 1;
            }
            TokenTree::Punct(punct)
                if typed
                    && punct.as_char() == '>'
                    && !punct_is(at.checked_sub(1).and_then(|i| tokens.get(i)), '-') =>
            {
                angles = angles.saturating_sub(1);
            }
            TokenTree::Group(group)
                if group.delimiter() == Delimiter::Brace
                    && matches!(
                        body,
                        Body::Module
                            | Body::Impl
                            | Body::Trait
                            | Body::Block
                            | Body::Arms
                            | Body::Foreign
                    ) =>
            {
                match tokens.get(at + 1) {
                    Some(next)
                        if punct_is(Some(next), ';')
                            || (body == Body::Arms && punct_is(Some(next), ',')) =>
                    {
                        at += 1;
                        end = Some(at);
                    }
                    // An item's body ends it, whatever follows: in a
                    // block, `mod m {…} ::a::b(…)` is two statements.
                    Some(next)
                        if continues_after_brace(next)
                            && (body == Body::Arms
                                || *kind.get_or_insert_with(|| item_kind(&tokens[start..at]))
                                    == Kind::Other) => {}
                    _ => end = Some(at),
                }
            }
            _ => {}
        }
        if let Some(end) = end {
            units.push(&tokens[start..=end]);
            start = end + 1;
            kind = None;
        }
        at += 1;
    }
    if start < tokens.len() {
        units.push(&tokens[start..]);
    }
    units
}

/// Whether reading `item` whole leaves its outer attributes unvisited, as
/// `Collector::visit_item` does for a module, a `use` and an `extern crate`.
fn attributes_unvisited(item: &[TokenTree]) -> bool {
    let words: Vec<String> = item
        .iter()
        .map_while(|token| match token {
            TokenTree::Ident(word) => Some(word.to_string()),
            TokenTree::Group(inner) if inner.delimiter() == Delimiter::Parenthesis => {
                Some(String::new())
            }
            _ => None,
        })
        .filter(|word| !word.is_empty() && word != "pub" && word != "unsafe")
        .take(2)
        .collect();
    match words.first().map(String::as_str) {
        Some("mod" | "use") => true,
        Some("extern") => words.get(1).is_some_and(|word| word == "crate"),
        _ => false,
    }
}

/// The outer attributes a unit over the budget can shed: all but those
/// the reader decides on (`cfg`, `cfg_attr`, `path`, and any ending in
/// `test`), as `(moved, the unit without them)`; `None` when there is none
/// to move.
fn movable_attributes(unit: &[TokenTree]) -> Option<(Vec<TokenTree>, Vec<TokenTree>)> {
    let mut moved = Vec::new();
    let mut kept = Vec::new();
    let mut at = 0;
    while punct_is(unit.get(at), '#')
        && matches!(unit.get(at + 1), Some(TokenTree::Group(inner)) if inner.delimiter() == Delimiter::Bracket)
    {
        let Some(TokenTree::Group(inner)) = unit.get(at + 1) else {
            break;
        };
        let words: Vec<String> = inner
            .stream()
            .into_iter()
            .take_while(|token| matches!(token, TokenTree::Ident(_) | TokenTree::Punct(_)))
            .filter_map(|token| match token {
                TokenTree::Ident(word) => Some(word.to_string()),
                _ => None,
            })
            .collect();
        let deciding = matches!(
            words.first().map(String::as_str),
            Some("cfg" | "cfg_attr" | "path")
        ) || words.last().is_some_and(|word| word == "test");
        let target = if deciding { &mut kept } else { &mut moved };
        target.extend(unit[at..at + 2].iter().cloned());
        at += 2;
    }
    if moved.is_empty() {
        return None;
    }
    kept.extend(unit[at..].iter().cloned());
    Some((moved, kept))
}

/// `fn __CodegraphHolder() where predicates {}`.
fn predicates_holder(predicates: TokenStream) -> TokenStream {
    [ident("fn"), ident(HOLDER)]
        .into_iter()
        .chain([
            group(Delimiter::Parenthesis, TokenStream::new()),
            ident("where"),
        ])
        .chain(predicates)
        .chain([group(Delimiter::Brace, TokenStream::new())])
        .collect()
}

/// The outer attributes (`#[…]` pairs) `tokens` starts with.
fn leading_attributes(tokens: &[TokenTree]) -> &[TokenTree] {
    let mut at = 0;
    while punct_is(tokens.get(at), '#')
        && matches!(tokens.get(at + 1), Some(TokenTree::Group(inner)) if inner.delimiter() == Delimiter::Bracket)
    {
        at += 2;
    }
    &tokens[..at]
}

/// `attributes struct __CodegraphHolder;`: outer attributes on an item
/// `syn` can read them on.
fn attribute_holder(attributes: &[TokenTree]) -> TokenStream {
    attributes
        .iter()
        .cloned()
        .chain([ident("struct"), ident(HOLDER)])
        .chain([TokenTree::Punct(proc_macro2::Punct::new(
            ';',
            Spacing::Alone,
        ))])
        .collect()
}

/// The name a wrapper gives what it wraps; never visited.
const HOLDER: &str = "__CodegraphHolder";

fn ident(word: &str) -> TokenTree {
    TokenTree::Ident(proc_macro2::Ident::new(word, Span::call_site()))
}

fn group(delimiter: Delimiter, stream: TokenStream) -> TokenTree {
    TokenTree::Group(proc_macro2::Group::new(delimiter, stream))
}

/// `words { inner }`.
fn wrapped(words: &[&str], inner: TokenStream) -> TokenStream {
    words
        .iter()
        .map(|word| ident(word))
        .chain([group(Delimiter::Brace, inner)])
        .collect()
}

fn stream_of(tokens: &[TokenTree]) -> TokenStream {
    tokens.iter().cloned().collect()
}

/// `tokens` with each outermost `{…}` that is no macro call's body emptied,
/// looking through `(…)` and `[…]`; the bodies taken out are pushed to
/// `holes` in order.
fn hollow_braces(tokens: &[TokenTree], holes: &mut Vec<(TokenStream, Body)>) -> Vec<TokenTree> {
    tokens
        .iter()
        .enumerate()
        .map(|(at, token)| match token {
            TokenTree::Group(inner) if !macro_body_at(tokens, at) => {
                let stream = if inner.delimiter() == Delimiter::Brace {
                    let content: Vec<TokenTree> = inner.stream().into_iter().collect();
                    let kind = hole_kind(&content);
                    holes.push((inner.stream(), kind));
                    TokenStream::new()
                } else {
                    let content: Vec<TokenTree> = inner.stream().into_iter().collect();
                    hollow_braces(&content, holes).into_iter().collect()
                };
                let mut emptied = proc_macro2::Group::new(inner.delimiter(), stream);
                emptied.set_span(inner.span());
                TokenTree::Group(emptied)
            }
            other => other.clone(),
        })
        .collect()
}

/// `tokens` with its largest `(…)` and `[…]` groups (no macro call's
/// body) emptied, largest first, until it fits [`MAX_SYN_TOKENS`]; their
/// contents, read as elements, are pushed to `holes`.
fn hollow_lists(tokens: &[TokenTree], holes: &mut Vec<(TokenStream, Body)>) -> Vec<TokenTree> {
    let mut tokens = tokens.to_vec();
    let mut sizes: Vec<(usize, usize)> = tokens
        .iter()
        .enumerate()
        .filter_map(|(at, token)| match token {
            TokenTree::Group(inner)
                if inner.delimiter() != Delimiter::Brace && !macro_body_at(&tokens, at) =>
            {
                let content: Vec<TokenTree> = inner.stream().into_iter().collect();
                Some((budget(&content), at))
            }
            _ => None,
        })
        .collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let mut total = budget(&tokens);
    for (size, at) in sizes {
        if total <= MAX_SYN_TOKENS {
            break;
        }
        if let TokenTree::Group(inner) = &tokens[at] {
            holes.push((inner.stream(), Body::Elements));
            let mut emptied = proc_macro2::Group::new(inner.delimiter(), TokenStream::new());
            emptied.set_span(inner.span());
            tokens[at] = TokenTree::Group(emptied);
            total -= size;
        }
    }
    tokens
}

/// How to read a `{…}` taken out of a statement: arms if it has a
/// top-level `=>`; struct-literal fields if it has a top-level `,` and no
/// `;`, or starts `name:`; else statements.
fn hole_kind(tokens: &[TokenTree]) -> Body {
    let top = |ch: char| tokens.iter().any(|token| punct_is(Some(token), ch));
    let arrow = tokens
        .windows(2)
        .any(|pair| punct_is(Some(&pair[0]), '=') && punct_is(Some(&pair[1]), '>'));
    let named_field = matches!(tokens.first(), Some(TokenTree::Ident(_)))
        && matches!(tokens.get(1), Some(TokenTree::Punct(punct))
            if punct.as_char() == ':' && punct.spacing() == Spacing::Alone);
    if arrow {
        Body::Arms
    } else if (top(',') && !top(';')) || named_field {
        Body::Fields
    } else {
        Body::Block
    }
}

/// Where a long flat run of tokens may be cut without cutting a path or a
/// type apart, and the length of the separator there: at `;`, at a `||` or
/// `&&` after a value, at an assignment's `=`, at `=>`, at a `{}` (a body
/// taken out), and before the keywords that start an expression's part.
fn segment_cut(tokens: &[TokenTree], at: usize) -> Option<usize> {
    let before = at.checked_sub(1).and_then(|index| tokens.get(index));
    let value_before = matches!(before, Some(TokenTree::Ident(_) | TokenTree::Literal(_)))
        || matches!(before, Some(TokenTree::Group(group)) if group.delimiter() != Delimiter::Brace)
        || punct_is(before, '?');
    match &tokens[at] {
        TokenTree::Punct(punct) => {
            let joint_with =
                |ch: char| punct.spacing() == Spacing::Joint && punct_is(tokens.get(at + 1), ch);
            let after_joint =
                matches!(before, Some(TokenTree::Punct(p)) if p.spacing() == Spacing::Joint);
            match punct.as_char() {
                ';' => Some(1),
                '|' if value_before && joint_with('|') => Some(2),
                '&' if value_before && joint_with('&') => Some(2),
                '=' if joint_with('>') => Some(2),
                '=' if punct.spacing() == Spacing::Alone && !after_joint => Some(1),
                _ => None,
            }
        }
        TokenTree::Group(group) if group.delimiter() == Delimiter::Brace => Some(1),
        TokenTree::Ident(word) => SEGMENT_KEYWORDS
            .iter()
            .any(|keyword| word == keyword)
            .then_some(0),
        _ => None,
    }
}

/// Keywords a segment starts at; all but `let` are dropped from it.
const SEGMENT_KEYWORDS: [&str; 9] = [
    "let", "if", "else", "while", "match", "return", "in", "for", "loop",
];

/// Reads a lexed file into a [`Collector`], handing `syn` at most
/// [`MAX_SYN_TOKENS`] tokens (plus the few of a wrapper) at a time.
#[derive(Default)]
struct Reader {
    collector: Collector,
    /// Some part was read with the token walk.
    fallback: bool,
    /// The most tokens any `syn` call was handed (without its wrapper).
    #[cfg(test)]
    largest: usize,
}

impl Reader {
    /// Read `tokens` as `body`, in chunks of whole units that fit the
    /// budget; a unit that does not fit is taken apart.
    fn read(&mut self, tokens: &[TokenTree], body: Body) {
        let mut pending: Vec<&[TokenTree]> = Vec::new();
        let mut size = 0;
        for unit in split(tokens, body) {
            let cost = budget(unit);
            if cost > MAX_SYN_TOKENS {
                self.flush(&mut pending, body);
                size = 0;
                self.large(unit, body);
                continue;
            }
            if size + cost > MAX_SYN_TOKENS {
                self.flush(&mut pending, body);
                size = 0;
            }
            pending.push(unit);
            size += cost;
        }
        self.flush(&mut pending, body);
    }

    /// Parse the pending units as one chunk; if `syn` refuses it, each unit
    /// alone, so one unit it refuses costs only itself.
    fn flush(&mut self, pending: &mut Vec<&[TokenTree]>, body: Body) {
        if pending.is_empty() {
            return;
        }
        let chunk: TokenStream = pending
            .iter()
            .flat_map(|unit| unit.iter().cloned())
            .collect();
        if pending.len() == 1 || !self.parse_chunk(chunk.clone(), body) {
            for unit in pending.iter() {
                if !self.parse_chunk(stream_of(unit), body) {
                    self.walk(unit, body);
                }
            }
        }
        pending.clear();
    }

    /// The one way tokens reach `syn`. `stream` is handed over with every
    /// macro call's body emptied, so `syn` reads exactly the tokens counted
    /// here, and is refused (`None`, nothing visited) past
    /// `MAX_SYN_TOKENS + WRAPPER_TOKENS`; `None` too when `syn` refuses it
    /// or `visit` declines what it parsed. While `visit` runs, the
    /// collector reads each macro's body from the original tokens; a body
    /// no macro claimed (a group taken for a macro's that is none) is
    /// walked afterwards and marks the file `token_fallback`.
    fn parse_with<T: syn::parse::Parse, R>(
        &mut self,
        stream: TokenStream,
        visit: impl FnOnce(&mut Collector, &T) -> Option<R>,
    ) -> Option<R> {
        let tokens: Vec<TokenTree> = stream.into_iter().collect();
        let mut bodies = BTreeMap::new();
        let mut in_attributes = BTreeSet::new();
        let stripped = strip_macro_bodies(&tokens, &mut bodies, false, &mut in_attributes);
        let count = token_count(&stripped);
        if count > MAX_SYN_TOKENS + WRAPPER_TOKENS {
            return None;
        }
        #[cfg(test)]
        {
            self.largest = self.largest.max(count);
        }
        let parsed = syn::parse2::<T>(stripped).ok()?;
        self.collector.stripped = bodies
            .into_iter()
            .map(|(key, body)| (key, (body, false)))
            .collect();
        let result = visit(&mut self.collector, &parsed);
        let unclaimed: Vec<TokenStream> = std::mem::take(&mut self.collector.stripped)
            .into_iter()
            // A macro call in an attribute the visitor does not read (a
            // list's tokens, a module's or file's attributes) is no group
            // mistaken for a macro's: it was never read.
            .filter_map(|(key, (body, claimed))| {
                (!claimed && !in_attributes.contains(&key)).then_some(body)
            })
            .collect();
        if result.is_some() && !unclaimed.is_empty() {
            self.fallback = true;
            let test = self.collector.in_test();
            let mut found = Vec::new();
            for body in unclaimed {
                walk_tokens(body, test, &mut found);
            }
            self.collector.current().paths.extend(found);
        }
        result
    }

    /// Parse `chunk` as `body` (wrapped as `syn` needs it) and visit what it
    /// holds; `false`, with nothing visited, when it is refused.
    fn parse_chunk(&mut self, chunk: TokenStream, body: Body) -> bool {
        match body {
            Body::Module => self.parse_with(chunk, |collector, file: &syn::File| {
                for item in &file.items {
                    collector.visit_item(item);
                }
                Some(())
            }),
            Body::Impl => self.parse_with(
                wrapped(&["impl", HOLDER], chunk),
                |collector, holder: &syn::ItemImpl| {
                    for item in &holder.items {
                        collector.visit_impl_item(item);
                    }
                    Some(())
                },
            ),
            Body::Trait => self.parse_with(
                wrapped(&["trait", HOLDER], chunk),
                |collector, holder: &syn::ItemTrait| {
                    for item in &holder.items {
                        collector.visit_trait_item(item);
                    }
                    Some(())
                },
            ),
            Body::Block => self.parse_with(wrapped(&[], chunk), |collector, block: &syn::Block| {
                collector.visit_block(block);
                Some(())
            }),
            Body::Arms => self.parse_with(
                wrapped(&["match", HOLDER], chunk),
                |collector, holder: &syn::ExprMatch| {
                    for arm in &holder.arms {
                        collector.visit_arm(arm);
                    }
                    Some(())
                },
            ),
            Body::Fields => self.parse_with(
                wrapped(&[HOLDER], chunk),
                |collector, holder: &syn::ExprStruct| {
                    for field in &holder.fields {
                        collector.visit_field_value(field);
                    }
                    if let Some(rest) = &holder.rest {
                        collector.visit_expr(rest);
                    }
                    Some(())
                },
            ),
            Body::FieldDefs => self.parse_with(
                wrapped(&["struct", HOLDER], chunk),
                |collector, holder: &syn::ItemStruct| {
                    for field in &holder.fields {
                        collector.visit_field(field);
                    }
                    Some(())
                },
            ),
            Body::Foreign => self.parse_with(
                wrapped(&["extern"], chunk),
                |collector, holder: &syn::ItemForeignMod| {
                    for item in &holder.items {
                        collector.visit_foreign_item(item);
                    }
                    Some(())
                },
            ),
            Body::Variants => self.parse_with(
                wrapped(&["enum", HOLDER], chunk),
                |collector, holder: &syn::ItemEnum| {
                    for variant in &holder.variants {
                        collector.visit_variant(variant);
                    }
                    Some(())
                },
            ),
            Body::Elements => self.parse_with(
                [group(Delimiter::Bracket, chunk)].into_iter().collect(),
                |collector, holder: &syn::ExprArray| {
                    for element in &holder.elems {
                        collector.visit_expr(element);
                    }
                    Some(())
                },
            ),
            Body::Predicates => self.parse_with(
                predicates_holder(chunk),
                |collector, holder: &syn::ItemFn| {
                    if let Some(clause) = &holder.sig.generics.where_clause {
                        collector.visit_where_clause(clause);
                    }
                    Some(())
                },
            ),
        }
        .is_some()
    }

    /// A unit over the budget: an item read as its header and its body,
    /// else the unit with its bodies and largest lists taken out, else
    /// its segments.
    fn large(&mut self, unit: &[TokenTree], body: Body) {
        if let Some((moved, kept)) = movable_attributes(unit) {
            self.attributes(&moved, &kept);
            if budget(&kept) > MAX_SYN_TOKENS {
                self.large(&kept, body);
            } else if !self.parse_chunk(stream_of(&kept), body) {
                self.walk(&kept, body);
            }
            return;
        }
        if matches!(body, Body::Module | Body::Impl | Body::Trait | Body::Block)
            && let Some((TokenTree::Group(inner), header)) = unit.split_last()
            && inner.delimiter() == Delimiter::Brace
            && self.item_with_body(header, inner, body)
        {
            return;
        }
        if body == Body::Arms && self.arm(unit) {
            return;
        }
        if matches!(body, Body::Module | Body::Block) && self.use_item(unit, body) {
            return;
        }
        let mut holes = Vec::new();
        let mut skeleton = hollow_braces(unit, &mut holes);
        if budget(&skeleton) > MAX_SYN_TOKENS {
            skeleton = hollow_lists(&skeleton, &mut holes);
        }
        if holes.is_empty()
            || budget(&skeleton) > MAX_SYN_TOKENS
            || !self.parse_chunk(stream_of(&skeleton), body)
        {
            self.segments(unit, body);
            return;
        }
        self.holes(holes);
    }

    /// A match arm over the budget whose pattern is a long `a | b | …`:
    /// the alternatives in chunks, each as an arm with an empty body, then
    /// the arm's own body. `false`, with nothing read, for any other arm.
    fn arm(&mut self, unit: &[TokenTree]) -> bool {
        let Some(arrow) = (1..unit.len()).find(|&at| {
            punct_is(unit.get(at), '>')
                && matches!(&unit[at - 1], TokenTree::Punct(punct) if punct.as_char() == '=' && punct.spacing() == Spacing::Joint)
        }) else {
            return false;
        };
        let pattern = &unit[..arrow - 1];
        if pattern.iter().any(|token| ident_is(Some(token), "if")) {
            return false;
        }
        let attributes = pattern
            .iter()
            .position(|token| {
                !punct_is(Some(token), '#')
                    && !matches!(token, TokenTree::Group(inner) if inner.delimiter() == Delimiter::Bracket)
            })
            .unwrap_or(pattern.len());
        let (attributes, pattern) = pattern.split_at(attributes);
        let mut alternatives: Vec<&[TokenTree]> = Vec::new();
        let mut start = 0;
        for (at, token) in pattern.iter().enumerate() {
            let joined_before = matches!(at.checked_sub(1).map(|i| &pattern[i]), Some(TokenTree::Punct(before)) if before.spacing() == Spacing::Joint);
            if let TokenTree::Punct(punct) = token
                && punct.as_char() == '|'
                && punct.spacing() == Spacing::Alone
                && !joined_before
            {
                alternatives.push(&pattern[start..at]);
                start = at + 1;
            }
        }
        alternatives.push(&pattern[start..]);
        alternatives.retain(|alternative| !alternative.is_empty());
        let head = budget(attributes) + 4;
        if alternatives.len() < 2
            || alternatives
                .iter()
                .any(|alternative| head + budget(alternative) > MAX_SYN_TOKENS)
        {
            return false;
        }
        let bar = || TokenTree::Punct(proc_macro2::Punct::new('|', Spacing::Alone));
        let as_arm = |chunk: &[&[TokenTree]]| -> (TokenStream, Vec<TokenTree>) {
            let mut tokens: Vec<TokenTree> = attributes.to_vec();
            for (index, alternative) in chunk.iter().enumerate() {
                if index > 0 {
                    tokens.push(bar());
                }
                tokens.extend(alternative.iter().cloned());
            }
            tokens.extend(unit[arrow - 1..=arrow].iter().cloned());
            tokens.push(group(Delimiter::Brace, TokenStream::new()));
            (stream_of(&tokens), tokens)
        };
        let mut chunk: Vec<&[TokenTree]> = Vec::new();
        let mut size = head;
        for alternative in alternatives {
            let cost = budget(alternative) + 1;
            if size + cost > MAX_SYN_TOKENS && !chunk.is_empty() {
                let (stream, tokens) = as_arm(&chunk);
                if !self.parse_chunk(stream, Body::Arms) {
                    self.walk(&tokens, Body::Arms);
                }
                chunk.clear();
                size = head;
            }
            chunk.push(alternative);
            size += cost;
        }
        let (stream, tokens) = as_arm(&chunk);
        if !self.parse_chunk(stream, Body::Arms) {
            self.walk(&tokens, Body::Arms);
        }
        let mut rest = &unit[arrow + 1..];
        if punct_is(rest.last(), ',') {
            rest = &rest[..rest.len() - 1];
        }
        self.collector.enter_block();
        self.read(rest, Body::Block);
        self.collector.leave_block();
        true
    }

    /// Read what was taken out of a statement or item: code nested in it.
    fn holes(&mut self, holes: Vec<(TokenStream, Body)>) {
        self.collector.enter_block();
        for (stream, kind) in holes {
            let tokens: Vec<TokenTree> = stream.into_iter().collect();
            self.read(&tokens, kind);
        }
        self.collector.leave_block();
    }

    /// The last resort for a unit no other way fits: cut it where no path
    /// or type is cut apart and read each segment as a statement (or as a
    /// `let` pattern); a segment that still does not fit, or that `syn`
    /// refuses, is read with the token walk. Its `{…}` bodies are read on
    /// their own. Every segment is read in the unit's own test scope. In a
    /// body of items the cut loses what the unit is (a `const` cut at its
    /// `=` is no longer one): the item's name is declared as the walk
    /// declares it and the file is marked `token_fallback`. A statement or
    /// expression cut at `;`, `=`, `||`, `&&`, `=>` or a keyword keeps its
    /// paths and bindings.
    fn segments(&mut self, unit: &[TokenTree], body: Body) {
        if !matches!(
            body,
            Body::Block | Body::Arms | Body::Fields | Body::Elements
        ) {
            self.fallback = true;
            self.declare_walked(unit, body);
        }
        let test = self.attributes_test(leading_attributes(unit));
        self.collector.test_depth += usize::from(test);
        let mut holes = Vec::new();
        let skeleton = hollow_braces(unit, &mut holes);
        let mut pieces: Vec<&[TokenTree]> = Vec::new();
        let (mut start, mut at) = (0, 0);
        while at < skeleton.len() {
            match segment_cut(&skeleton, at) {
                Some(length) => {
                    pieces.push(&skeleton[start..at]);
                    start = at + length;
                    at = start.max(at + 1);
                }
                None => at += 1,
            }
        }
        pieces.push(&skeleton[start.min(skeleton.len())..]);
        for mut piece in pieces {
            while let Some(TokenTree::Ident(word)) = piece.first()
                && word != "let"
                && SEGMENT_KEYWORDS.iter().any(|keyword| word == keyword)
            {
                piece = &piece[1..];
            }
            if piece.is_empty() {
                continue;
            }
            let as_statement: TokenStream = stream_of(piece)
                .into_iter()
                .chain([TokenTree::Punct(proc_macro2::Punct::new(
                    ';',
                    Spacing::Alone,
                ))])
                .collect();
            let as_pattern: TokenStream = [ident("let")]
                .into_iter()
                .chain(as_statement.clone())
                .collect();
            let declares = piece[leading_attributes(piece).len()..]
                .iter()
                .take(3)
                .any(|token| ident_is(Some(token), "const") || ident_is(Some(token), "static"));
            if declares
                || budget(piece) > MAX_SYN_TOKENS
                || !(self.parse_chunk(as_statement, Body::Block)
                    || self.parse_chunk(as_pattern, Body::Block))
            {
                self.walk(piece, Body::Block);
            }
        }
        self.holes(holes);
        self.collector.test_depth -= usize::from(test);
    }

    /// A `use` item over the budget (`use a::{…};`, any attributes and
    /// visibility before it): its group read in chunks, each as
    /// `use a::{chunk};`. `false`, with nothing read, for any other unit.
    fn use_item(&mut self, unit: &[TokenTree], body: Body) -> bool {
        let Some(keyword) = unit.iter().position(|token| ident_is(Some(token), "use")) else {
            return false;
        };
        let before_is_head = unit[..keyword].iter().all(|token| match token {
            TokenTree::Punct(punct) => punct.as_char() == '#',
            TokenTree::Group(inner) => inner.delimiter() != Delimiter::Brace,
            TokenTree::Ident(word) => word == "pub",
            TokenTree::Literal(_) => false,
        });
        if !before_is_head {
            return false;
        }
        let mut tree = &unit[keyword + 1..];
        if punct_is(tree.last(), ';') {
            tree = &tree[..tree.len() - 1];
        }
        self.use_tree(&unit[..=keyword], tree, body);
        true
    }

    /// Read `use head tree;` where `head` ends in `use` or `::`: when it is
    /// over the budget and `tree` is `prefix::{…}`, the group's trees in
    /// chunks, a tree too large for one chunk taken one level down;
    /// otherwise as one item, walked when `syn` refuses it.
    fn use_tree(&mut self, head: &[TokenTree], tree: &[TokenTree], body: Body) {
        let semicolon = || TokenTree::Punct(proc_macro2::Punct::new(';', Spacing::Alone));
        let whole = |head: &[TokenTree], tree: &[TokenTree]| -> Vec<TokenTree> {
            head.iter()
                .chain(tree)
                .cloned()
                .chain([semicolon()])
                .collect()
        };
        let group_last = matches!(tree.last(), Some(TokenTree::Group(inner)) if inner.delimiter() == Delimiter::Brace);
        let prefix_is_path = tree[..tree.len().saturating_sub(1)]
            .iter()
            .all(|token| matches!(token, TokenTree::Ident(_)) || punct_is(Some(token), ':'));
        let item = whole(head, tree);
        if budget(&item) <= MAX_SYN_TOKENS || !group_last || !prefix_is_path {
            if budget(&item) > MAX_SYN_TOKENS || !self.parse_chunk(stream_of(&item), body) {
                self.walk(&item, body);
            }
            return;
        }
        let Some((TokenTree::Group(inner), prefix)) = tree.split_last() else {
            return;
        };
        let base: Vec<TokenTree> = head.iter().chain(prefix).cloned().collect();
        let content: Vec<TokenTree> = inner.stream().into_iter().collect();
        let leaves: Vec<&[TokenTree]> = content
            .split(|token| punct_is(Some(token), ','))
            .filter(|leaf| !leaf.is_empty())
            .collect();
        let comma = || TokenTree::Punct(proc_macro2::Punct::new(',', Spacing::Alone));
        let base_cost = budget(&base) + 2;
        let mut chunk: Vec<TokenTree> = Vec::new();
        let mut size = base_cost;
        let emit = |reader: &mut Self, chunk: &mut Vec<TokenTree>| {
            if chunk.is_empty() {
                return;
            }
            let mut braced = proc_macro2::Group::new(Delimiter::Brace, stream_of(chunk));
            braced.set_span(inner.span());
            let item: Vec<TokenTree> = base
                .iter()
                .cloned()
                .chain([TokenTree::Group(braced), semicolon()])
                .collect();
            if !reader.parse_chunk(stream_of(&item), body) {
                reader.walk(&item, body);
            }
            chunk.clear();
        };
        for leaf in leaves {
            let cost = budget(leaf) + 1;
            if base_cost + cost > MAX_SYN_TOKENS {
                emit(self, &mut chunk);
                size = base_cost;
                self.use_tree(&base, leaf, body);
                continue;
            }
            if size + cost > MAX_SYN_TOKENS {
                emit(self, &mut chunk);
                size = base_cost;
            }
            chunk.extend(leaf.iter().cloned());
            chunk.push(comma());
            size += cost;
        }
        emit(self, &mut chunk);
    }

    /// An item over the budget whose last token is its `{…}` body: its
    /// header (with an empty body) parsed and visited, then its body read
    /// in the item's own context. `false` when it is no such item or `syn`
    /// refuses the header.
    fn item_with_body(
        &mut self,
        header: &[TokenTree],
        inner: &proc_macro2::Group,
        body: Body,
    ) -> bool {
        let kind = item_kind(header);
        if kind == Kind::Other {
            return false;
        }
        let content: Vec<TokenTree> = inner.stream().into_iter().collect();
        let (head, predicates) = match header
            .iter()
            .position(|token| ident_is(Some(token), "where"))
        {
            Some(at) if budget(header) > MAX_SYN_TOKENS => (&header[..at], Some(&header[at + 1..])),
            _ => (header, None),
        };
        if budget(head) > MAX_SYN_TOKENS {
            return false;
        }
        let mut emptied = proc_macro2::Group::new(Delimiter::Brace, TokenStream::new());
        emptied.set_span(inner.span());
        let stream: TokenStream = head
            .iter()
            .cloned()
            .chain([TokenTree::Group(emptied)])
            .collect();
        let read = match body {
            Body::Module | Body::Block => self.module_item(stream, kind, &content),
            Body::Impl | Body::Trait if kind == Kind::Fn => self.method(stream, body, &content),
            _ => false,
        };
        if read && let Some(predicates) = predicates {
            self.read(predicates, Body::Predicates);
        }
        read
    }

    fn module_item(&mut self, header: TokenStream, kind: Kind, content: &[TokenTree]) -> bool {
        let read = self.parse_with(header, |collector, file: &syn::File| {
            let [item] = file.items.as_slice() else {
                return None;
            };
            let test = is_test_item(item_attrs(item));
            let inner = match (kind, item) {
                (Kind::Mod, Item::Mod(module)) => {
                    return Some((Some(ModHeader::of(module)), test, Body::Module));
                }
                (Kind::Impl, Item::Impl(_)) => Body::Impl,
                (Kind::Trait, Item::Trait(_)) => Body::Trait,
                (Kind::Fn, Item::Fn(_)) => Body::Block,
                (Kind::Struct, Item::Struct(_)) | (Kind::Union, Item::Union(_)) => Body::FieldDefs,
                (Kind::Enum, Item::Enum(_)) => Body::Variants,
                (Kind::Foreign, Item::ForeignMod(_)) => Body::Foreign,
                _ => return None,
            };
            collector.visit_item(item);
            Some((None, test, inner))
        });
        let Some((module, test, inner)) = read else {
            return false;
        };
        match module {
            Some(module) => {
                self.collector.test_depth += usize::from(test);
                self.collector.enter_module();
                self.read(content, Body::Module);
                let facts = self.collector.leave_module();
                self.collector.declare_mod(module, test, Some(facts));
                self.collector.test_depth -= usize::from(test);
            }
            None if inner == Body::Block => self.in_test(test, |reader| reader.block_body(content)),
            None => self.in_test(test, |reader| reader.read(content, inner)),
        }
        true
    }

    /// A method over the budget in an `impl` or trait body.
    fn method(&mut self, header: TokenStream, body: Body, content: &[TokenTree]) -> bool {
        let test = if body == Body::Impl {
            self.parse_with(
                wrapped(&["impl", HOLDER], header),
                |collector, holder: &syn::ItemImpl| {
                    let [item @ syn::ImplItem::Fn(method)] = holder.items.as_slice() else {
                        return None;
                    };
                    collector.visit_impl_item(item);
                    Some(is_test_item(&method.attrs))
                },
            )
        } else {
            self.parse_with(
                wrapped(&["trait", HOLDER], header),
                |collector, holder: &syn::ItemTrait| {
                    let [item @ syn::TraitItem::Fn(_)] = holder.items.as_slice() else {
                        return None;
                    };
                    collector.visit_trait_item(item);
                    Some(false)
                },
            )
        };
        let Some(test) = test else {
            return false;
        };
        self.in_test(test, |reader| reader.block_body(content));
        true
    }

    /// Whether outer attributes (`#[…]` pairs) make what they are on test
    /// code (`#[cfg(test)]`, `#[test]`); `false` when they cannot be read.
    fn attributes_test(&mut self, attributes: &[TokenTree]) -> bool {
        if attributes.is_empty() {
            return false;
        }
        self.parse_with(
            attribute_holder(attributes),
            |_, holder: &syn::ItemStruct| Some(is_test_item(&holder.attrs)),
        )
        .unwrap_or(false)
    }

    /// Outer attributes moved off an item over the budget, visited in
    /// chunks on a holder item, in the test scope of the item they belong
    /// to (whose deciding attributes stayed on it: `kept`).
    fn attributes(&mut self, moved: &[TokenTree], kept: &[TokenTree]) {
        let deciding = leading_attributes(kept);
        // As a whole file is read: a module's, `use`'s or `extern crate`'s
        // attributes are not visited.
        if attributes_unvisited(&kept[deciding.len()..]) {
            return;
        }
        let test = self.attributes_test(deciding);
        self.collector.test_depth += usize::from(test);
        let mut pending: Vec<TokenTree> = Vec::new();
        let mut size = 0;
        for pair in moved.chunks(2) {
            let cost = budget(pair);
            if size + cost > MAX_SYN_TOKENS && !pending.is_empty() {
                self.attribute_chunk(&pending);
                pending.clear();
                size = 0;
            }
            pending.extend(pair.iter().cloned());
            size += cost;
        }
        if !pending.is_empty() {
            self.attribute_chunk(&pending);
        }
        self.collector.test_depth -= usize::from(test);
    }

    /// Visit outer attributes on a holder item; walked when refused (one
    /// attribute over the budget alone, or one `syn` cannot read).
    fn attribute_chunk(&mut self, attributes: &[TokenTree]) {
        let read = self.parse_with(
            attribute_holder(attributes),
            |collector, holder: &syn::ItemStruct| {
                for attribute in &holder.attrs {
                    collector.visit_attribute(attribute);
                }
                Some(())
            },
        );
        if read.is_none() {
            self.walk(attributes, Body::Block);
        }
    }

    fn block_body(&mut self, content: &[TokenTree]) {
        self.collector.enter_block();
        self.read(content, Body::Block);
        self.collector.leave_block();
    }

    fn in_test(&mut self, test: bool, read: impl FnOnce(&mut Self)) {
        self.collector.test_depth += usize::from(test);
        read(self);
        self.collector.test_depth -= usize::from(test);
    }

    /// The token walk for a piece `syn` cannot be handed: its `a::b`
    /// paths (in the test scope its own attributes give it, too), and at
    /// module level its `mod name;` and its item's name.
    fn walk(&mut self, tokens: &[TokenTree], body: Body) {
        self.fallback = true;
        let test = self.collector.in_test() || self.attributes_test(leading_attributes(tokens));
        let mut found = Vec::new();
        walk_tokens(stream_of(tokens), test, &mut found);
        self.collector.current().paths.extend(found);
        self.declare_walked(tokens, body);
    }

    /// At module level, what the walk can tell `tokens` declares: its
    /// `mod name;` and its item's name (after `fn`, `struct`, `const`, …).
    fn declare_walked(&mut self, tokens: &[TokenTree], body: Body) {
        if body != Body::Module || !self.collector.at_module_level() {
            return;
        }
        token_mods(stream_of(tokens), self.collector.current());
        let name = tokens.windows(2).find_map(|pair| match pair {
            [TokenTree::Ident(keyword), TokenTree::Ident(name)]
                if [
                    "fn", "struct", "enum", "union", "trait", "type", "const", "static",
                ]
                .iter()
                .any(|word| keyword == word) =>
            {
                Some(ident_text(name))
            }
            _ => None,
        });
        if let Some(name) = name {
            self.collector.current().items.insert(name);
        }
    }
}

/// Read `text`: lexed with `proc_macro2` once, then read item by item and
/// statement by statement, `syn` never handed more than
/// [`MAX_SYN_TOKENS`] tokens at a time (see [`Reader`]). A part `syn`
/// refuses (syntax newer than it knows) or that cannot be cut to the budget
/// is read with a token walk that still finds its paths, and marks the
/// file's facts `token_fallback`. `Err` (listed as `unparsed`) when the
/// brackets nest past [`MAX_NESTING`] or the text does not lex.
pub(crate) fn parse(text: &str) -> Result<FileFacts, String> {
    let deepest = bracket_depth(text);
    if deepest > MAX_NESTING {
        return Err(format!(
            "brackets nest {deepest} deep, over the limit of {MAX_NESTING}: not lexed or parsed"
        ));
    }
    let tokens: Vec<TokenTree> = TokenStream::from_str(source_text(text))
        .map_err(|error| format!("does not lex: {error}"))?
        .into_iter()
        .collect();
    let deepest = group_depth(&tokens);
    if deepest > MAX_NESTING {
        return Err(format!(
            "brackets nest {deepest} deep, over the limit of {MAX_NESTING}: not parsed"
        ));
    }
    let mut reader = Reader::default();
    reader.read(&tokens, Body::Module);
    Ok(FileFacts {
        root: reader.collector.finish(),
        token_fallback: reader.fallback,
    })
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte >= 0x80
}

fn starts_comment(bytes: &[u8], at: usize) -> bool {
    bytes[at] == b'/' && matches!(bytes.get(at + 1), Some(b'/' | b'*'))
}

/// Past the comment that starts at `at`.
fn skip_comment(bytes: &[u8], mut at: usize) -> usize {
    if bytes.get(at + 1) == Some(&b'*') {
        return skip_block_comment(bytes, at + 2);
    }
    while at < bytes.len() && bytes[at] != b'\n' {
        at += 1;
    }
    at
}

/// Past a (nested) block comment whose `/*` ends before `at`.
fn skip_block_comment(bytes: &[u8], mut at: usize) -> usize {
    let mut level = 1;
    while at < bytes.len() && level > 0 {
        if bytes[at] == b'/' && bytes.get(at + 1) == Some(&b'*') {
            level += 1;
            at += 2;
        } else if bytes[at] == b'*' && bytes.get(at + 1) == Some(&b'/') {
            level -= 1;
            at += 2;
        } else {
            at += 1;
        }
    }
    at
}

/// Past an escaped literal whose opening quote ends before `at`.
fn skip_quoted(bytes: &[u8], mut at: usize, quote: u8) -> usize {
    while at < bytes.len() {
        match bytes[at] {
            b'\\' => at += 2,
            byte if byte == quote => return at + 1,
            _ => at += 1,
        }
    }
    at
}

/// Past a char literal (`'x'`, `'\\n'`, `'\\u{1F600}'`, `'é'`) at `at`, with
/// `true`; or past a lifetime or label (`'a`, `'static`), with `false`.
fn skip_char_or_lifetime(bytes: &[u8], at: usize) -> (usize, bool) {
    match bytes.get(at + 1) {
        Some(b'\\') => (skip_quoted(bytes, at + 3, b'\''), true),
        Some(&lead) => {
            let width = match lead {
                0xF0..=0xFF => 4,
                0xE0..=0xEF => 3,
                0xC0..=0xDF => 2,
                _ => 1,
            };
            if bytes.get(at + 1 + width) == Some(&b'\'') {
                (at + 2 + width, true)
            } else {
                let mut end = at + 1;
                while end < bytes.len() && is_ident_byte(bytes[end]) {
                    end += 1;
                }
                (end, false)
            }
        }
        None => (at + 1, false),
    }
}

/// Past a `r"…"`, `r#"…"#`, `b"…"`, `br#"…"#`, `c"…"` or `cr"…"` literal at
/// `at`; `None` when the letters start an identifier instead.
fn skip_prefixed_string(bytes: &[u8], at: usize) -> Option<usize> {
    let mut next = at;
    if matches!(bytes[next], b'b' | b'c') {
        next += 1;
    }
    let raw = bytes.get(next) == Some(&b'r');
    if raw {
        next += 1;
    }
    let hashes = bytes[next..]
        .iter()
        .take_while(|byte| **byte == b'#')
        .count();
    next += hashes;
    if bytes.get(next) != Some(&b'"') || (!raw && (hashes > 0 || next == at)) {
        return None;
    }
    if !raw {
        return Some(skip_quoted(bytes, next + 1, b'"'));
    }
    let mut end = next + 1;
    while end < bytes.len() {
        if bytes[end] == b'"'
            && bytes[end + 1..].iter().take_while(|b| **b == b'#').count() >= hashes
        {
            return Some(end + 1 + hashes);
        }
        end += 1;
    }
    Some(end)
}

/// What a `mod name` item says besides its body.
struct ModHeader {
    name: String,
    line: u32,
    path_attr: Option<String>,
}

impl ModHeader {
    fn of(item: &syn::ItemMod) -> Self {
        Self {
            name: ident_text(&item.ident),
            line: line_of(item.ident.span()),
            path_attr: item.attrs.iter().find_map(path_attribute),
        }
    }
}

#[derive(Default)]
struct Collector {
    /// The module being read and its enclosing inline modules.
    stack: Vec<ModuleFacts>,
    /// Nesting of blocks inside the current module: items in a block are
    /// local, not the module's.
    blocks: Vec<usize>,
    test_depth: usize,
    /// The macro bodies emptied out of what `syn` is reading now, by span,
    /// each with whether a macro claimed it (see `Reader::parse_with`).
    stripped: BTreeMap<SpanKey, (TokenStream, bool)>,
}

impl Collector {
    fn current(&mut self) -> &mut ModuleFacts {
        if self.stack.is_empty() {
            self.stack.push(ModuleFacts::default());
            self.blocks.push(0);
        }
        self.stack.last_mut().expect("a module")
    }

    fn finish(mut self) -> ModuleFacts {
        self.current();
        self.stack.pop().unwrap_or_default()
    }

    fn in_test(&self) -> bool {
        self.test_depth > 0
    }

    fn at_module_level(&self) -> bool {
        self.blocks.last().is_none_or(|depth| *depth == 0)
    }

    fn declare(&mut self, ident: &syn::Ident) {
        if self.at_module_level() {
            let name = ident.to_string();
            self.current().items.insert(name);
        }
    }

    fn module(&mut self, item: &syn::ItemMod, test: bool) {
        let inline = item.content.as_ref().map(|(_, items)| {
            self.enter_module();
            for inner in items {
                self.visit_item(inner);
            }
            self.leave_module()
        });
        self.declare_module(item, test, inline);
    }

    /// Start reading an inline module's body.
    fn enter_module(&mut self) {
        self.current();
        self.stack.push(ModuleFacts::default());
        self.blocks.push(0);
    }

    /// Finish reading an inline module's body: its facts.
    fn leave_module(&mut self) -> ModuleFacts {
        self.blocks.pop();
        self.stack.pop().unwrap_or_default()
    }

    /// Code nested in a block from here: its items are local.
    fn enter_block(&mut self) {
        self.current();
        if let Some(depth) = self.blocks.last_mut() {
            *depth += 1;
        }
    }

    fn leave_block(&mut self) {
        if let Some(depth) = self.blocks.last_mut() {
            *depth = depth.saturating_sub(1);
        }
    }

    /// A macro call's body: the original tokens when `syn` was handed it
    /// emptied.
    fn macro_tokens(&mut self, mac: &syn::Macro) -> TokenStream {
        let key = span_key(mac.delimiter.span().join());
        match self.stripped.get_mut(&key) {
            Some((body, claimed)) if mac.tokens.is_empty() => {
                *claimed = true;
                body.clone()
            }
            _ => self.restore(mac.tokens.clone()),
        }
    }

    /// `tokens` (a macro body `syn` was handed in full, as `try!(…)`'s)
    /// with each macro body emptied inside it put back, and claimed.
    fn restore(&mut self, tokens: TokenStream) -> TokenStream {
        if self.stripped.is_empty() {
            return tokens;
        }
        tokens
            .into_iter()
            .map(|token| match token {
                TokenTree::Group(inner) => {
                    let stream = match self.stripped.get_mut(&span_key(inner.span())) {
                        Some((body, claimed)) if inner.stream().is_empty() => {
                            *claimed = true;
                            body.clone()
                        }
                        _ => self.restore(inner.stream()),
                    };
                    let mut restored = proc_macro2::Group::new(inner.delimiter(), stream);
                    restored.set_span(inner.span());
                    TokenTree::Group(restored)
                }
                other => other,
            })
            .collect()
    }

    /// Record `mod name` (with its inline body's facts, if any) in the
    /// current module.
    fn declare_module(&mut self, item: &syn::ItemMod, test: bool, inline: Option<ModuleFacts>) {
        self.declare_mod(ModHeader::of(item), test, inline);
    }

    fn declare_mod(&mut self, header: ModHeader, test: bool, inline: Option<ModuleFacts>) {
        let ModHeader {
            name,
            line,
            path_attr,
        } = header;
        if self.at_module_level() {
            let test = test || self.in_test();
            self.current().mods.push(ModDecl {
                name,
                line,
                test,
                path_attr,
                inline,
            });
        }
    }

    fn use_item(&mut self, item: &syn::ItemUse) {
        let public = !matches!(item.vis, syn::Visibility::Inherited);
        let mut leaves = Vec::new();
        expand_use(
            &item.tree,
            &mut Vec::new(),
            line_of(item.use_token.span),
            &mut leaves,
        );
        let test = self.in_test();
        let leading_colon = item.leading_colon.is_some();
        for (segments, name, glob, line) in leaves {
            self.current().uses.push(UseEntry {
                segments,
                name,
                glob,
                public,
                leading_colon,
                line,
                test,
            });
        }
    }

    fn extern_crate(&mut self, item: &syn::ItemExternCrate) {
        let krate = ident_text(&item.ident);
        if krate == "self" {
            return;
        }
        let name = item
            .rename
            .as_ref()
            .map_or_else(|| krate.clone(), |(_, rename)| ident_text(rename));
        let entry = UseEntry {
            segments: vec![krate],
            name: (name != "_").then_some(name),
            glob: false,
            public: !matches!(item.vis, syn::Visibility::Inherited),
            leading_colon: true,
            line: line_of(item.ident.span()),
            test: self.in_test(),
        };
        self.current().uses.push(entry);
    }

    fn include(&mut self, mac: &syn::Macro) {
        let Some(last) = mac.path.segments.last() else {
            return;
        };
        let name = last.ident.to_string();
        if !matches!(name.as_str(), "include" | "include_str" | "include_bytes") {
            return;
        }
        let line = line_of(last.ident.span());
        if let Some((path, from_manifest_dir)) = include_target(self.macro_tokens(mac)) {
            let test = self.in_test();
            self.current().includes.push(IncludeRef {
                path,
                from_manifest_dir,
                line,
                test,
            });
        }
    }
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_item(&mut self, item: &'ast Item) {
        let test = is_test_item(item_attrs(item));
        if test {
            self.test_depth += 1;
        }
        match item {
            Item::Mod(module) => self.module(module, test),
            Item::Use(item_use) => self.use_item(item_use),
            Item::ExternCrate(item_extern) => self.extern_crate(item_extern),
            _ => {
                match item {
                    Item::Fn(f) => self.declare(&f.sig.ident),
                    Item::Struct(s) => self.declare(&s.ident),
                    Item::Enum(e) => self.declare(&e.ident),
                    Item::Union(u) => self.declare(&u.ident),
                    Item::Trait(t) => self.declare(&t.ident),
                    Item::TraitAlias(t) => self.declare(&t.ident),
                    Item::Const(c) => self.declare(&c.ident),
                    Item::Static(s) => self.declare(&s.ident),
                    Item::Type(t) => self.declare(&t.ident),
                    Item::Macro(m) => {
                        if let Some(ident) = &m.ident {
                            self.declare(ident);
                        } else if self.at_module_level() {
                            let names = flatten_idents(self.macro_tokens(&m.mac));
                            self.current().macro_items.extend(names);
                        }
                    }
                    _ => {}
                }
                visit::visit_item(self, item);
            }
        }
        if test {
            self.test_depth -= 1;
        }
    }

    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let test = is_test_item(&item.attrs);
        if test {
            self.test_depth += 1;
        }
        visit::visit_impl_item_fn(self, item);
        if test {
            self.test_depth -= 1;
        }
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        // The module first: a block read before anything else in a file
        // must not find the depth pushed under it during the visit.
        self.current();
        if let Some(depth) = self.blocks.last_mut() {
            *depth += 1;
        }
        visit::visit_block(self, block);
        if let Some(depth) = self.blocks.last_mut() {
            *depth = depth.saturating_sub(1);
        }
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        if let (1, false, Some(segment)) = (
            path.segments.len(),
            path.leading_colon.is_some(),
            path.segments.first(),
        ) {
            let line = line_of(segment.ident.span());
            let test = self.in_test();
            // The first line, a production use preferred: independent of
            // the order the reader visits the file's parts in.
            let entry = self
                .current()
                .local_refs
                .entry(ident_text(&segment.ident))
                .or_insert((line, test));
            if (entry.1 && !test) || (entry.1 == test && line < entry.0) {
                *entry = (line, test);
            }
        }
        if path.segments.len() >= 2 || path.leading_colon.is_some() {
            let line = path
                .segments
                .first()
                .map_or(0, |segment| line_of(segment.ident.span()));
            let entry = PathRef {
                segments: path
                    .segments
                    .iter()
                    .map(|segment| ident_text(&segment.ident))
                    .collect(),
                leading_colon: path.leading_colon.is_some(),
                line,
                test: self.in_test(),
            };
            self.current().paths.push(entry);
        }
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        visit::visit_macro(self, mac);
        self.include(mac);
        let test = self.in_test();
        let mut found = Vec::new();
        walk_tokens(self.macro_tokens(mac), test, &mut found);
        self.current().paths.extend(found);
    }
}

/// An identifier without its raw `r#` prefix.
fn ident_text(ident: &syn::Ident) -> String {
    let text = ident.to_string();
    text.strip_prefix("r#").map_or(text.clone(), str::to_string)
}

fn item_attrs(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs,
        Item::Enum(i) => &i.attrs,
        Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs,
        Item::ForeignMod(i) => &i.attrs,
        Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs,
        Item::Mod(i) => &i.attrs,
        Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs,
        Item::Trait(i) => &i.attrs,
        Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs,
        Item::Union(i) => &i.attrs,
        Item::Use(i) => &i.attrs,
        _ => &[],
    }
}

/// `#[cfg(test)]` (not under `not(...)`), or a test function attribute
/// (`#[test]`, `#[tokio::test]`).
fn is_test_item(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.is_ident("cfg") {
            let Ok(list) = attr.meta.require_list() else {
                return false;
            };
            let words: Vec<String> = flatten_idents(list.tokens.clone());
            words.iter().any(|word| word == "test") && !words.iter().any(|word| word == "not")
        } else {
            path.segments
                .last()
                .is_some_and(|segment| segment.ident == "test")
        }
    })
}

/// Every identifier in a token stream, groups included. Iterative.
fn flatten_idents(tokens: TokenStream) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = vec![tokens];
    while let Some(stream) = pending.pop() {
        for token in stream {
            match token {
                TokenTree::Ident(ident) => out.push(ident.to_string()),
                TokenTree::Group(group) => pending.push(group.stream()),
                _ => {}
            }
        }
    }
    out
}

/// `#[path = "..."]`.
fn path_attribute(attr: &Attribute) -> Option<String> {
    if !attr.path().is_ident("path") {
        return None;
    }
    let syn::Meta::NameValue(pair) = &attr.meta else {
        return None;
    };
    match &pair.value {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Str(text),
            ..
        }) => Some(text.value()),
        _ => None,
    }
}

/// Expand a `use` tree into `(segments, name, glob, line)` leaves.
fn expand_use(
    tree: &UseTree,
    prefix: &mut Vec<String>,
    line: u32,
    out: &mut Vec<(Vec<String>, Option<String>, bool, u32)>,
) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(ident_text(&path.ident));
            expand_use(&path.tree, prefix, line_of(path.ident.span()), out);
            prefix.pop();
        }
        UseTree::Name(name) => {
            let ident = ident_text(&name.ident);
            let line = line_of(name.ident.span());
            if ident == "self" {
                out.push((prefix.clone(), prefix.last().cloned(), false, line));
            } else {
                let mut segments = prefix.clone();
                segments.push(ident.clone());
                out.push((segments, Some(ident), false, line));
            }
        }
        UseTree::Rename(rename) => {
            let ident = ident_text(&rename.ident);
            let alias = ident_text(&rename.rename);
            let mut segments = prefix.clone();
            if ident != "self" {
                segments.push(ident);
            }
            let name = (alias != "_").then_some(alias);
            out.push((segments, name, false, line_of(rename.ident.span())));
        }
        UseTree::Glob(_) => out.push((prefix.clone(), None, true, line)),
        UseTree::Group(group) => {
            for item in &group.items {
                expand_use(item, prefix, line, out);
            }
        }
    }
}

/// The path of `include*!(...)`: a single string literal (relative to the
/// file), or `concat!(env!("CARGO_MANIFEST_DIR"), "...")` (relative to the
/// package root).
fn include_target(tokens: TokenStream) -> Option<(String, bool)> {
    let top: Vec<TokenTree> = tokens.into_iter().collect();
    if let [TokenTree::Literal(literal)] = top.as_slice() {
        return string_literal(literal).map(|path| (path, false));
    }
    let mut literals = Vec::new();
    let mut idents = Vec::new();
    collect_literals(top.into_iter().collect(), &mut literals, &mut idents);
    if idents.iter().any(|ident| ident == "concat")
        && idents.iter().any(|ident| ident == "env")
        && literals
            .first()
            .is_some_and(|first| first == "CARGO_MANIFEST_DIR")
    {
        let rest: String = literals[1..].concat();
        let rest = rest.trim_start_matches('/').to_string();
        return (!rest.is_empty()).then_some((rest, true));
    }
    None
}

fn collect_literals(tokens: TokenStream, literals: &mut Vec<String>, idents: &mut Vec<String>) {
    for token in tokens {
        match token {
            TokenTree::Literal(literal) => {
                if let Some(text) = string_literal(&literal) {
                    literals.push(text);
                }
            }
            TokenTree::Ident(ident) => idents.push(ident.to_string()),
            TokenTree::Group(group) => collect_literals(group.stream(), literals, idents),
            TokenTree::Punct(_) => {}
        }
    }
}

fn string_literal(literal: &proc_macro2::Literal) -> Option<String> {
    match syn::Lit::new(literal.clone()) {
        syn::Lit::Str(text) => Some(text.value()),
        _ => None,
    }
}

/// The `mod name;` items of a token stream (the fallback for a file `syn`
/// refuses), at the top level only.
fn token_mods(tokens: TokenStream, root: &mut ModuleFacts) {
    let top: Vec<TokenTree> = tokens.into_iter().collect();
    for window in top.windows(3) {
        if let [
            TokenTree::Ident(keyword),
            TokenTree::Ident(name),
            TokenTree::Punct(semi),
        ] = window
            && keyword == "mod"
            && semi.as_char() == ';'
        {
            root.mods.push(ModDecl {
                name: name.to_string(),
                line: line_of(name.span()),
                test: false,
                path_attr: None,
                inline: None,
            });
        }
    }
}

/// Every `ident (:: ident)+` sequence (and `:: ident ...`) in a token
/// stream and in each of its groups (iteratively, with a stack of groups).
/// A walk over lexed tokens: text inside string literals and comments is
/// never seen.
fn walk_tokens(tokens: TokenStream, test: bool, out: &mut Vec<PathRef>) {
    let mut pending = vec![tokens];
    while let Some(stream) = pending.pop() {
        walk_level(stream, test, out, &mut pending);
    }
}

/// The paths of one level of a token stream; its groups go to `pending`.
fn walk_level(
    tokens: TokenStream,
    test: bool,
    out: &mut Vec<PathRef>,
    pending: &mut Vec<TokenStream>,
) {
    enum Tok {
        Ident(String, u32),
        PathSep,
        Other,
    }
    let mut flat: Vec<Tok> = Vec::new();
    let mut iter = tokens.into_iter().peekable();
    while let Some(token) = iter.next() {
        match token {
            TokenTree::Ident(ident) => {
                flat.push(Tok::Ident(ident_text(&ident), line_of(ident.span())));
            }
            TokenTree::Punct(punct)
                if punct.as_char() == ':' && punct.spacing() == Spacing::Joint =>
            {
                if matches!(iter.peek(), Some(TokenTree::Punct(next)) if next.as_char() == ':') {
                    iter.next();
                    flat.push(Tok::PathSep);
                } else {
                    flat.push(Tok::Other);
                }
            }
            TokenTree::Group(group) => {
                if group.delimiter() != Delimiter::None || !group.stream().is_empty() {
                    pending.push(group.stream());
                }
                flat.push(Tok::Other);
            }
            _ => flat.push(Tok::Other),
        }
    }
    let mut at = 0;
    while at < flat.len() {
        let leading_colon = matches!(flat[at], Tok::PathSep)
            && matches!(flat.get(at + 1), Some(Tok::Ident(..)))
            && (at == 0 || !matches!(flat[at - 1], Tok::Ident(..)));
        let start = if leading_colon { at + 1 } else { at };
        let Some(Tok::Ident(first, line)) = flat.get(start) else {
            at += 1;
            continue;
        };
        let mut segments = vec![first.clone()];
        let mut next = start + 1;
        while let (Some(Tok::PathSep), Some(Tok::Ident(segment, _))) =
            (flat.get(next), flat.get(next + 1))
        {
            segments.push(segment.clone());
            next += 2;
        }
        if segments.len() >= 2 || leading_colon {
            out.push(PathRef {
                segments,
                leading_colon,
                line: *line,
                test,
            });
        }
        at = next;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_trees_expand_with_aliases_globs_and_self() {
        let facts = parse(
            "use crate::a::{self, b::C as D, e::*};\npub use super::f;\nuse ::ext::g as _;\nextern crate core_x as cx;\n",
        )
        .expect("parse");
        let uses: Vec<_> = facts
            .root
            .uses
            .iter()
            .map(|u| {
                (
                    u.segments.join("::"),
                    u.name.clone(),
                    u.glob,
                    u.public,
                    u.line,
                )
            })
            .collect();
        assert_eq!(
            uses,
            vec![
                (
                    "crate::a".to_string(),
                    Some("a".to_string()),
                    false,
                    false,
                    1
                ),
                (
                    "crate::a::b::C".to_string(),
                    Some("D".to_string()),
                    false,
                    false,
                    1
                ),
                ("crate::a::e".to_string(), None, true, false, 1),
                (
                    "super::f".to_string(),
                    Some("f".to_string()),
                    false,
                    true,
                    2
                ),
                ("ext::g".to_string(), None, false, false, 3),
                (
                    "core_x".to_string(),
                    Some("cx".to_string()),
                    false,
                    false,
                    4
                ),
            ]
        );
        assert!(facts.root.uses[4].leading_colon && facts.root.uses[5].leading_colon);
    }

    #[test]
    fn macro_bodies_give_paths_but_strings_and_comments_do_not() {
        let facts = parse(
            "fn f() {\n    json!({ \"k\": crate::util::clamp(1), \"s\": \"crate::no::path\" });\n    // crate::nor::this\n    m!(::abs::x, $crate::y::z);\n}\n",
        )
        .expect("parse");
        let paths: Vec<_> = facts
            .root
            .paths
            .iter()
            .map(|p| (p.segments.join("::"), p.leading_colon, p.line))
            .collect();
        assert_eq!(
            paths,
            vec![
                ("crate::util::clamp".to_string(), false, 2),
                ("abs::x".to_string(), true, 4),
                ("crate::y::z".to_string(), false, 4),
            ]
        );
        assert!(facts.root.items.contains("f"));
    }

    #[test]
    fn items_in_blocks_are_local_and_inline_modules_nest() {
        let facts = parse(
            "pub struct S;\nfn g() { struct Local; }\n#[cfg(test)]\nmod tests { use super::S; fn t() {} }\n#[path = \"x/y.rs\"]\nmod z;\n",
        )
        .expect("parse");
        assert_eq!(
            facts.root.items,
            BTreeSet::from(["S".to_string(), "g".to_string()])
        );
        let tests = &facts.root.mods[0];
        assert!(tests.test);
        let inner = facts.root.nested(&["tests".to_string()]).expect("inline");
        assert!(inner.items.contains("t"));
        assert!(inner.uses[0].test);
        assert_eq!(facts.root.mods[1].path_attr.as_deref(), Some("x/y.rs"));
        assert_eq!(facts.root.mods[1].line, 6);
    }

    #[test]
    fn a_file_syn_refuses_falls_back_to_tokens() {
        let facts = parse("mod a;\nfn f() { crate::b::c( }\n").expect_err("unbalanced");
        assert!(!facts.is_empty());
        let facts = parse("mod a;\nfn f() -> impl use<'a> + ?? { crate::b::c() }\n")
            .expect("token fallback");
        assert!(facts.token_fallback);
        assert_eq!(facts.root.mods[0].name, "a");
        assert_eq!(facts.root.paths[0].segments.join("::"), "crate::b::c");
    }

    /// Read `text` as [`parse`] does, with the most tokens any `syn` call
    /// got.
    fn read_counting(text: &str) -> (FileFacts, usize) {
        let text = text.to_string();
        // On a thread with the parser thread's stack, as `resolve` reads.
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let tokens: Vec<TokenTree> = TokenStream::from_str(source_text(&text))
                    .expect("lexes")
                    .into_iter()
                    .collect();
                let mut reader = Reader::default();
                reader.read(&tokens, Body::Module);
                let largest = reader.largest;
                let facts = FileFacts {
                    root: reader.collector.finish(),
                    token_fallback: reader.fallback,
                };
                (facts, largest)
            })
            .expect("a parser thread")
            .join()
            .expect("no panic")
    }

    fn joined(count: usize, piece: impl Fn(usize) -> String) -> String {
        (0..count).map(piece).collect()
    }

    #[test]
    fn a_file_past_the_budget_is_read_in_pieces_with_every_fact() {
        let statements = joined(400, |i| format!("let v{i} = w.f({i});\n"));
        let text = format!(
            "{docs}use crate::a::A;\n\
             pub struct Big {{ {fields} last: crate::x::T }}\n\
             pub enum E {{ {variants} Last(crate::y::U) }}\n\
             impl Big {{ pub fn m(&self) {{ {statements} crate::m::last(); }} }}\n\
             mod inner {{\n    pub fn f() {{ {statements} super::g(); }}\n    #[cfg(test)]\n    mod tests {{\n        #[test]\n        fn t() {{ {statements} crate::t::only(); }}\n    }}\n}}\n\
             const TABLE: &[(&str, u8)] = &[{table}];\n\
             fn guards(x: u8) -> u8 {{ match x {{ {arms} _ => Local::one() }} }}\n",
            docs = joined(300, |i| format!("//! doc line {i}\n")),
            fields = joined(300, |i| format!("f{i}: u8, ")),
            variants = joined(300, |i| format!("V{i}, ")),
            table = joined(400, |i| format!("(\"k{i}\", {i}), ")),
            arms = joined(300, |i| format!("v if v < {i} => {i}, ")),
        );
        let (facts, largest) = read_counting(&text);
        assert!(largest <= MAX_SYN_TOKENS + WRAPPER_TOKENS, "{largest}");
        assert!(!facts.token_fallback);
        let root = &facts.root;
        for name in ["Big", "E", "guards", "TABLE"] {
            assert!(root.items.contains(name), "{name}: {:?}", root.items);
        }
        let paths: Vec<(String, bool)> = root
            .paths
            .iter()
            .map(|p| (p.segments.join("::"), p.test))
            .collect();
        for path in ["crate::x::T", "crate::y::U", "crate::m::last", "Local::one"] {
            assert!(
                paths.contains(&(path.to_string(), false)),
                "{path}: {paths:?}"
            );
        }
        assert_eq!(root.uses[0].segments.join("::"), "crate::a::A");
        assert!(root.local_refs.contains_key("w"));
        let inner = root.nested(&["inner".to_string()]).expect("inline module");
        assert!(inner.items.contains("f"));
        assert!(
            inner
                .paths
                .iter()
                .any(|p| p.segments.join("::") == "super::g")
        );
        assert!(root.mods[0].inline.is_some() && !root.mods[0].test);
        let tests = inner.nested(&["tests".to_string()]).expect("nested module");
        assert!(inner.mods[0].test);
        assert!(tests.items.contains("t"));
        assert!(
            tests
                .paths
                .iter()
                .any(|p| p.segments.join("::") == "crate::t::only" && p.test)
        );
    }

    #[test]
    fn no_syn_call_gets_more_than_the_budget() {
        let body = |unit: &str, count: usize, end: &str| {
            format!(
                "fn f(c: bool, y: u8) {{ let _ = {}{end}; }}\n",
                unit.repeat(count)
            )
        };
        for text in [
            body("&", 6_000, "c"),
            body("!", 6_000, "c"),
            body("|a,b| ", 6_000, "1"),
            body("#[a] |a,b| ", 6_000, "1"),
            body("|S{x},b| ", 6_000, "1"),
            body("|const{0},b| ", 6_000, "1"),
            body("", 1, &format!("x{}", "::<{0},B> = x".repeat(12_000))),
            body("", 1, &format!("x{}", "::<A,B> = x".repeat(12_000))),
            format!("type T = {}u8;\n", "for<'a,'b> fn() -> ".repeat(3_000)),
            format!("type T = {}u8;\n", "&".repeat(6_000)),
            body("y", 1, &".f(1, 2)".repeat(12_000)),
            body("if c { 1 }", 1, &" else if c { 2 }".repeat(6_000)),
            body("{ 1 }", 1, &" + { 1 }".repeat(6_000)),
            format!("{}fn f() {{}}\n", "#[doc = \"x\"] #[a(b)] ".repeat(3_000)),
            format!("{}mod m {{}}\n", "#[a::b] ".repeat(6_000)),
            format!(
                "fn f(c: char) -> u8 {{ match c {{ 'a'{} => 1, _ => 0 }} }}\n",
                " | 'b'".repeat(6_000)
            ),
            format!(
                "fn f(c: &&u8) -> u8 {{ match c {{ {}x | y => 1 }} }}\n",
                "&".repeat(6_000)
            ),
            format!("extern \"C\" {{ {} }}\n", "fn c(x: u8);".repeat(3_000)),
        ] {
            let (_, largest) = read_counting(&text);
            assert!(
                largest <= MAX_SYN_TOKENS + WRAPPER_TOKENS,
                "{largest}: {}",
                &text[..60]
            );
        }
    }

    #[test]
    fn a_macro_calls_body_counts_once_and_other_groups_in_full() {
        let count = |text: &str| {
            let tokens: Vec<TokenTree> = TokenStream::from_str(text)
                .expect("lexes")
                .into_iter()
                .collect();
            budget(&tokens)
        };
        let items = "1, ".repeat(2_000);
        assert_eq!(count(&format!("vec![{items}]")), 3);
        assert_eq!(count(&format!("macro_rules! m {{ {items} }}")), 4);
        assert!(count(&format!("if !({items}) {{}}")) > 4_000);
        assert!(count(&format!("x != ({items})")) > 4_000);
        assert!(count(&format!("[{items}]")) > 4_000);
        // A label or lifetime before `!` names no macro.
        assert!(count(&format!("break 'a !({items})")) > 4_000);
        assert!(count(&format!("break 'macro_rules !m({items})")) > 4_000);
    }

    #[test]
    fn a_const_cut_into_segments_keeps_its_name_and_its_test_scope() {
        let chain = joined(100, |_| " && crate::target::hit()".to_owned());
        for (text, test) in [
            (
                format!("const A: bool = crate::target::hit(){chain};\nfn f() {{}}\n"),
                false,
            ),
            (
                format!(
                    "#[cfg(test)]\nconst A: bool = crate::target::hit(){chain};\nfn f() {{}}\n"
                ),
                true,
            ),
            (
                format!("pub static A: bool = crate::target::hit(){chain};\n"),
                false,
            ),
        ] {
            let (facts, largest) = read_counting(&text);
            assert!(largest <= MAX_SYN_TOKENS + WRAPPER_TOKENS, "{largest}");
            assert!(facts.token_fallback, "{text:.40}");
            assert!(facts.root.items.contains("A"), "{:?}", facts.root.items);
            assert_eq!(facts.root.paths.len(), 101, "{text:.40}");
            assert!(
                facts
                    .root
                    .paths
                    .iter()
                    .all(|path| path.test == test
                        && path.segments.join("::") == "crate::target::hit"),
                "{text:.40}"
            );
        }
    }

    /// The facts of a whole-file parse that empties no macro body: the
    /// oracle the reader must match on valid code. Test only.
    fn whole_file(text: &str) -> ModuleFacts {
        let text = text.to_string();
        std::thread::Builder::new()
            .stack_size(64 * 1024 * 1024)
            .spawn(move || {
                let file = syn::parse_file(&text).expect("valid Rust");
                let mut collector = Collector::default();
                for item in &file.items {
                    collector.visit_item(item);
                }
                collector.finish()
            })
            .expect("a parser thread")
            .join()
            .expect("no panic")
    }

    #[test]
    fn macro_calls_the_visitor_never_reads_are_no_fallback() {
        // Macro calls in attributes the reader does not visit (a file's,
        // a module's or a `use`'s, an attribute list's tokens), in visited
        // ones, and nested in a keyword-named macro's body: the facts of a
        // whole-file parse that empties nothing, and no token walk.
        for text in [
            "#![doc = include_str!(\"../README.md\")]\npub mod target;\nmod probe;\n",
            "#[doc = include_str!(\"x.md\")]\npub mod m;\n",
            "#[doc = include_str!(\"x.md\")]\npub mod m { pub fn f() { crate::a::b(); } }\n",
            "#[doc = include_str!(\"x.md\")]\npub use crate::a::b;\n",
            "#[doc = concat!(\"x\", \"y\")]\nextern crate alloc;\n",
            "#[cfg_attr(docsrs, doc = concat!(\"a\", \"b\"))]\npub fn f() {}\n",
            "#[case(datetime!(2020-01-01 x::y))]\nfn t() {}\n",
            "#[doc = include_str!(\"x.md\")]\npub fn f() { crate::a::b(); }\n",
            "fn f() -> Result<(), E> {\n    try!(g(format!(\"{}\", x::y)));\n    Ok(())\n}\n",
            "fn f() { r#try!(g(vec![x::y(), m!(z::w)])); }\n",
        ] {
            let (facts, _) = read_counting(text);
            assert!(!facts.token_fallback, "{text}");
            assert_eq!(facts.root, whole_file(text), "{text}");
        }
        let (facts, _) = read_counting(
            "fn f() -> Result<(), E> {\n    try!(g(format!(\"{}\", x::y)));\n    Ok(())\n}\n",
        );
        assert!(
            facts
                .root
                .paths
                .iter()
                .any(|path| path.segments.join("::") == "x::y"),
            "{:?}",
            facts.root.paths
        );
    }

    #[test]
    fn a_const_in_a_body_cut_into_segments_keeps_its_type_paths() {
        let chain = joined(300, |i| format!(" && y::f{i}()"));
        for text in [
            format!("fn f() {{ const A: crate::target::T = y::g(){chain}; }}\n"),
            format!(
                "struct S;\nimpl S {{ fn m() {{ static B: crate::target::T = y::g(){chain}; }} }}\n"
            ),
            format!("fn f() {{ let c = || {{ const C: crate::target::T = y::g(){chain}; }}; }}\n"),
        ] {
            let (facts, largest) = read_counting(&text);
            assert!(largest <= MAX_SYN_TOKENS + WRAPPER_TOKENS, "{largest}");
            assert!(facts.token_fallback, "{text:.60}");
            let found: BTreeSet<String> = facts
                .root
                .paths
                .iter()
                .map(|path| path.segments.join("::"))
                .collect();
            for path in whole_file(&text).paths {
                assert!(
                    found.contains(&path.segments.join("::")),
                    "{path:?} in {text:.60}"
                );
            }
            assert!(found.contains("crate::target::T"), "{found:?}");
        }
    }

    #[test]
    fn a_macro_body_is_read_from_the_original_tokens() {
        // `syn` gets each macro's body emptied; the facts come from the
        // tokens that were taken out.
        let facts = parse(
            "fn f() { m!(crate::a::b(), include!(\"x.rs\")); }\n\
             n! { crate::c::d }\n\
             include!(\"y.rs\");\n",
        )
        .expect("parses");
        assert!(!facts.token_fallback);
        let paths: Vec<String> = facts
            .root
            .paths
            .iter()
            .map(|path| path.segments.join("::"))
            .collect();
        assert!(paths.contains(&"crate::a::b".to_owned()), "{paths:?}");
        assert!(paths.contains(&"crate::c::d".to_owned()), "{paths:?}");
        assert!(
            facts.root.macro_items.contains("d"),
            "{:?}",
            facts.root.macro_items
        );
        let includes: Vec<&str> = facts
            .root
            .includes
            .iter()
            .map(|i| i.path.as_str())
            .collect();
        assert_eq!(includes, vec!["y.rs"]);
    }

    #[test]
    fn a_first_line_is_a_shebang_or_an_inner_attribute() {
        assert_eq!(source_text("#!/usr/bin/env x\nfn f() {}"), "\nfn f() {}");
        assert_eq!(source_text("\u{feff}#!/bin/x \"\nfn f() {}"), "\nfn f() {}");
        assert_eq!(
            source_text(concat!("#!", "[allow(unused)]\nfn f() {}")),
            concat!("#!", "[allow(unused)]\nfn f() {}")
        );
        assert_eq!(
            source_text("#! /* c */ [a]\nfn f() {}"),
            "#! /* c */ [a]\nfn f() {}"
        );
        let facts = parse("#!/usr/bin/env x \"\nfn f() { crate::a::b(); }\n").expect("parses");
        assert_eq!(facts.root.paths[0].segments.join("::"), "crate::a::b");
        let facts = parse(concat!("#!", "[allow(unused)]\nmod m;\n")).expect("parses");
        assert_eq!(facts.root.mods[0].name, "m");
        let deep = format!(
            "fn f() {{ let _ = {}1{}; }}\n",
            "(".repeat(10_000),
            ")".repeat(10_000)
        );
        for head in ["#!/bin/x \"\n", "\u{feff}#!/bin/x \"\n", "#!/bin/x /*\n"] {
            let error = parse(&format!("{head}{deep}")).expect_err("too deep");
            assert!(error.contains("over the limit of 256"), "{error}");
        }
    }

    #[test]
    fn brackets_are_measured_outside_comments_strings_and_chars() {
        let at_limit = format!("fn main() {}1{}", "(".repeat(255), ")".repeat(255));
        assert_eq!(bracket_depth(&at_limit), 255);
        let opens = "(".repeat(100_000);
        for hidden in [
            format!("// {opens}\nfn f() {{}}\n"),
            format!("/* /* {opens} */ {opens} */ fn f() {{}}\n"),
            format!("const A: &str = \"{opens} \\\" {opens}\";\n"),
            format!("const R: &str = r#\"{opens} \" {opens}\"#;\n"),
            format!("const B: &[u8] = br\"{opens}\";\nconst C: &[u8] = b\"{opens}\";\n"),
            format!(
                "const P: char = '(';\nconst Q: char = '\\'';\nfn f<'a>(x: &'a str) -> &'a str {{ x }}\n// {opens}\n"
            ),
        ] {
            assert!(bracket_depth(&hidden) <= 2, "{}", &hidden[..60]);
            parse(&hidden).expect("a valid file with brackets in comments and strings parses");
        }
    }

    #[test]
    fn item_level_macro_calls_name_possible_items() {
        let facts = parse(
            "macro_rules! label { ($name:ident) => { pub struct $name; }; }\nlabel!(SessionId);\nfn f() { label!(Local); }\n",
        )
        .expect("parse");
        assert!(facts.root.items.contains("label"));
        assert!(facts.root.macro_items.contains("SessionId"));
        assert!(
            !facts.root.macro_items.contains("Local"),
            "a call in a block is local"
        );
    }

    #[test]
    fn single_segment_names_are_kept_with_their_first_line() {
        let facts =
            parse("#[cfg(test)]\nfn t() -> Thing { Thing }\nfn f(x: Thing) {}\n").expect("parse");
        assert_eq!(facts.root.local_refs.get("Thing"), Some(&(3, false)));
    }

    #[test]
    fn include_paths_are_literal_or_manifest_relative() {
        let facts = parse(
            "const A: &str = include_str!(\"../sql/a.sql\");\nconst B: &[u8] = include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/tests/data/b.bin\"));\nconst C: &str = include_str!(some_macro!());\n",
        )
        .expect("parse");
        let includes: Vec<_> = facts
            .root
            .includes
            .iter()
            .map(|i| (i.path.as_str(), i.from_manifest_dir, i.line))
            .collect();
        assert_eq!(
            includes,
            vec![("../sql/a.sql", false, 1), ("tests/data/b.bin", true, 2)]
        );
    }
}
