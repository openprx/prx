use super::helpers::{
    blank_comments_and_literals, enclosing_function, is_ident_char, normalize_line, read_source, relative_path,
    rust_source_files, strip_cfg_test_items,
};
use super::loop_requests::{is_test_code_file, test_module_files};
use super::tool_tables::{
    NON_TOOL_CLOSURE_PROVIDERS, NON_TOOL_RECEIVER_PRODUCERS, NON_TOOL_RECEIVER_TYPES, NonToolCallSite,
    PRODUCER_INERT_TAIL_METHODS, TOOL_EXECUTION_METHODS,
};
use std::collections::BTreeMap;
use std::collections::BTreeSet;
/// The receiver identifier of a method call whose `.` sits at `dot`.
///
/// Walks back over `?`, balanced call/index brackets and whitespace, which is
/// what makes a multi-line chain (`self\n    .inner\n    .execute(args)`) and a
/// trailing accessor (`checkout.client_mut()?.execute(...)`) reduce to the same
/// identifier a single-line call would. A receiver the walk cannot reduce is
/// reported as `<expression>`, which is not in `NON_TOOL_EXECUTE_RECEIVERS` and
/// therefore counts as a tool execution.
fn receiver_identifier(characters: &[char], dot: usize) -> String {
    let mut index = dot;
    loop {
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        if index == 0 {
            return "<expression>".to_string();
        }
        let previous = characters[index - 1];
        if previous == '?' {
            index -= 1;
            continue;
        }
        if previous == ')' || previous == ']' {
            let (open, close) = if previous == ')' { ('(', ')') } else { ('[', ']') };
            let mut depth = 0usize;
            let mut scan = index - 1;
            loop {
                if characters[scan] == close {
                    depth += 1;
                } else if characters[scan] == open {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
                if scan == 0 {
                    return "<expression>".to_string();
                }
                scan -= 1;
            }
            index = scan;
            continue;
        }
        if is_ident_char(previous) {
            let mut start = index - 1;
            while start > 0 && is_ident_char(characters[start - 1]) {
                start -= 1;
            }
            return characters[start..index].iter().collect();
        }
        return "<expression>".to_string();
    }
}

/// The receiver expression as it is written: `self.tool_execution`, `state.tool`,
/// `conn`.
///
/// The bare identifier is not enough to review a call site — `conn` could be a
/// pooled database handle or a field holding a `Tool` — so the scanner keeps the
/// dotted path that produced it. Anything it cannot reduce to a dotted
/// identifier path (a call, an index, `?`) falls back to
/// [`receiver_identifier`], which collapses those to `<expression>`.
fn receiver_expression(characters: &[char], dot: usize) -> String {
    let mut segments: Vec<String> = Vec::new();
    let mut index = dot;
    loop {
        while index > 0 && characters[index - 1].is_whitespace() {
            index -= 1;
        }
        if index == 0 || !is_ident_char(characters[index - 1]) {
            break;
        }
        let mut start = index - 1;
        while start > 0 && is_ident_char(characters[start - 1]) {
            start -= 1;
        }
        segments.push(characters[start..index].iter().collect());
        let mut joint = start;
        while joint > 0 && characters[joint - 1].is_whitespace() {
            joint -= 1;
        }
        if joint == 0 || characters[joint - 1] != '.' {
            break;
        }
        index = joint - 1;
    }
    if segments.is_empty() {
        return receiver_identifier(characters, dot);
    }
    segments.reverse();
    segments.join(".")
}

/// Whether the receiver of the call at `dot` is a plain local name - something
/// a `let`, a parameter or a closure parameter in this function could have
/// bound - rather than a field, an index or the result of another call.
///
/// `conn.execute(..)` is a local name. `self.conn.execute(..)`,
/// `state.tool.execute(..)` and `checkout.client_mut()?.execute(..)` are not:
/// their type lives on a struct or on another function's return type, so the
/// binding rule has nothing to say about them and they fall through to the
/// reviewed site exceptions.
const fn receiver_is_local_binding(characters: &[char], dot: usize) -> bool {
    let mut index = dot;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 || !is_ident_char(characters[index - 1]) {
        return false;
    }
    let mut start = index - 1;
    while start > 0 && is_ident_char(characters[start - 1]) {
        start -= 1;
    }
    let mut before = start;
    while before > 0 && characters[before - 1].is_whitespace() {
        before -= 1;
    }
    if before == 0 {
        return true;
    }
    if matches!(characters[before - 1], '.' | ')' | ']' | '?') {
        return false;
    }
    // A path segment (`crate::THING.execute(..)`), not a local. A single colon
    // is a macro label or a struct field and says nothing either way.
    !(characters[before - 1] == ':' && before >= 2 && characters[before - 2] == ':')
}

/// The type or trait a UFCS call names: `Tool::execute`, `MessageSendTool::execute`
/// and `<Self as Tool>::execute` all reduce to one segment.
fn qualified_path_segment(characters: &[char], colons: usize) -> String {
    let mut index = colons;
    while index > 0 && characters[index - 1].is_whitespace() {
        index -= 1;
    }
    if index == 0 {
        return "<path>".to_string();
    }
    let previous = characters[index - 1];
    if previous == '>' {
        let mut depth = 0usize;
        let mut scan = index - 1;
        loop {
            if characters[scan] == '>' {
                depth += 1;
            } else if characters[scan] == '<' {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            if scan == 0 {
                return "<path>".to_string();
            }
            scan -= 1;
        }
        let inner: String = characters[scan + 1..index - 1].iter().collect();
        let qualified = inner.rsplit(" as ").next().unwrap_or(&inner).trim().to_string();
        return qualified.rsplit("::").next().unwrap_or(&qualified).trim().to_string();
    }
    if is_ident_char(previous) {
        let mut start = index - 1;
        while start > 0 && is_ident_char(characters[start - 1]) {
            start -= 1;
        }
        return characters[start..index].iter().collect();
    }
    "<path>".to_string()
}

/// The production text of the function that contains `line_index`.
///
/// The same backwards scan [`enclosing_function`] uses to name the function,
/// followed forwards to the closing brace, so the binding rule below reads
/// exactly the body the call site sits in.
fn enclosing_function_body(lines: &[&str], line_index: usize) -> Option<String> {
    let mut start = None;
    for (offset, line) in lines[..=line_index].iter().enumerate().rev() {
        let Some(fn_offset) = line.find("fn ") else {
            continue;
        };
        let name: String = line[fn_offset + 3..]
            .chars()
            .take_while(|character| is_ident_char(*character))
            .collect();
        if !name.is_empty() {
            start = Some(offset);
            break;
        }
    }
    let start = start?;
    let text = lines[start..].join("\n");
    let mut depth = 0usize;
    let mut opened = false;
    for (offset, character) in text.char_indices() {
        match character {
            '{' => {
                depth += 1;
                opened = true;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                if opened && depth == 0 {
                    return Some(text[..=offset].to_string());
                }
            }
            _ => {}
        }
    }
    Some(text)
}

/// Read a type annotation starting at `from`, stopping at the first `stops`
/// character that is not nested inside `<>`, `()` or `[]`.
fn type_annotation(characters: &[char], from: usize, stops: &[char]) -> String {
    let mut depth = 0i32;
    let mut text = String::new();
    for character in characters.iter().skip(from) {
        if depth <= 0 && stops.contains(character) {
            break;
        }
        match character {
            '<' | '(' | '[' => depth += 1,
            '>' | ')' | ']' => depth -= 1,
            _ => {}
        }
        text.push(*character);
        if text.len() > 200 {
            break;
        }
    }
    normalize_line(&text)
}

/// Read an initialiser expression starting at `from`, stopping at the statement
/// terminator that is not nested inside a bracket.
fn expression_text(characters: &[char], from: usize) -> String {
    let mut depth = 0i32;
    let mut text = String::new();
    for character in characters.iter().skip(from) {
        match character {
            '<' | '(' | '[' | '{' => depth += 1,
            '>' | ')' | ']' | '}' => depth -= 1,
            ';' if depth <= 0 => break,
            _ => {}
        }
        text.push(*character);
        if text.len() > 400 {
            break;
        }
    }
    normalize_line(&text)
}

/// Whether the identifier at `position` sits on the binding side of a `let`,
/// `if let` or `while let` - a destructuring pattern - rather than in the
/// initialiser or in an ordinary call argument.
fn is_let_pattern_position(characters: &[char], position: usize) -> bool {
    let mut index = position;
    while index > 0 {
        let character = characters[index - 1];
        if character == ';' || character == '{' || character == '}' {
            return false;
        }
        if character == '=' {
            let previous = if index >= 2 { characters[index - 2] } else { ' ' };
            let next = characters.get(index).copied().unwrap_or(' ');
            if !matches!(next, '=' | '>') && !matches!(previous, '=' | '!' | '<' | '>') {
                return false;
            }
        }
        if is_ident_char(character) {
            let mut word_start = index - 1;
            while word_start > 0 && is_ident_char(characters[word_start - 1]) {
                word_start -= 1;
            }
            let word: String = characters[word_start..index].iter().collect();
            if word == "let" {
                return true;
            }
            index = word_start;
            continue;
        }
        index -= 1;
    }
    false
}

/// The callee of the call whose argument list contains `position`.
///
/// `with_connection(config, |conn| ...)` resolves to `with_connection`, which is
/// how an untyped closure parameter recovers a type.
fn enclosing_call_name(characters: &[char], position: usize) -> Option<String> {
    let mut depth = 0i32;
    let mut index = position;
    while index > 0 {
        index -= 1;
        match characters[index] {
            ')' | ']' => depth += 1,
            '(' | '[' if depth > 0 => depth -= 1,
            '(' => {
                let mut end = index;
                while end > 0 && characters[end - 1].is_whitespace() {
                    end -= 1;
                }
                if end == 0 || !is_ident_char(characters[end - 1]) {
                    return None;
                }
                let mut start = end;
                while start > 0 && is_ident_char(characters[start - 1]) {
                    start -= 1;
                }
                return Some(characters[start..end].iter().collect());
            }
            '[' | '{' | '}' | ';' => return None,
            _ => {}
        }
    }
    None
}

/// What one occurrence of the receiver identifier says about the receiver's
/// type, or `None` when the occurrence is a use rather than a binding.
///
/// A binding form the scanner cannot read yields an `<unparsed ...>` marker that
/// no table approves, so the call site falls through to review instead of being
/// guessed at.
fn classify_receiver_binding(characters: &[char], start: usize, end: usize) -> Option<String> {
    let mut before = start;
    let mut keyword: Option<String> = None;
    loop {
        while before > 0 && characters[before - 1].is_whitespace() {
            before -= 1;
        }
        if before == 0 {
            return None;
        }
        if !is_ident_char(characters[before - 1]) {
            break;
        }
        let mut word_start = before - 1;
        while word_start > 0 && is_ident_char(characters[word_start - 1]) {
            word_start -= 1;
        }
        let candidate: String = characters[word_start..before].iter().collect();
        if candidate == "mut" || candidate == "ref" {
            before = word_start;
            continue;
        }
        keyword = Some(candidate);
        break;
    }

    let mut after = end;
    while after < characters.len() && characters[after].is_whitespace() {
        after += 1;
    }
    let annotated = characters.get(after) == Some(&':') && characters.get(after + 1) != Some(&':');

    if let Some(keyword) = keyword {
        return match keyword.as_str() {
            "let" => Some(if annotated {
                format!("let type: {}", type_annotation(characters, after + 1, &['=', ';']))
            } else if characters.get(after) == Some(&'=') {
                format!("let value: {}", expression_text(characters, after + 1))
            } else {
                "<unparsed let binding>".to_string()
            }),
            "for" => Some("<for-loop binding>".to_string()),
            _ => None,
        };
    }

    let previous = characters[before - 1];
    match previous {
        '(' | ',' if annotated => Some(format!(
            "parameter: {}",
            type_annotation(characters, after + 1, &[',', ')', '|'])
        )),
        '|' if annotated => Some(format!(
            "closure parameter: {}",
            type_annotation(characters, after + 1, &[',', '|'])
        )),
        '|' if before < 2 || characters[before - 2] != '|' => {
            Some(enclosing_call_name(characters, before - 1).map_or_else(
                || "<untyped closure parameter>".to_string(),
                |callee| format!("closure parameter of {callee}"),
            ))
        }
        '(' | ',' if is_let_pattern_position(characters, before - 1) => Some("<unparsed let binding>".to_string()),
        _ => None,
    }
}

/// Every binding of `name` the scanner can see inside one function body.
fn receiver_bindings(body: &str, name: &str) -> Vec<String> {
    let characters: Vec<char> = body.chars().collect();
    let mut bindings = Vec::new();
    let mut index = 0usize;
    while index < characters.len() {
        if !is_ident_char(characters[index]) || (index > 0 && is_ident_char(characters[index - 1])) {
            index += 1;
            continue;
        }
        let mut end = index;
        while end < characters.len() && is_ident_char(characters[end]) {
            end += 1;
        }
        if characters[index..end].iter().collect::<String>() == name
            && let Some(binding) = classify_receiver_binding(&characters, index, end)
        {
            bindings.push(binding);
        }
        index = end;
    }
    bindings
}

/// The bare type name a type annotation resolves to: `&mut Connection`,
/// `&rusqlite::Transaction<'_>` and `PooledConnection<'_>` all reduce to one
/// identifier, and a wrapper such as `Arc<dyn Tool>` reduces to `Arc`, which no
/// table approves.
fn base_type_identifier(annotation: &str) -> String {
    let mut text = annotation.trim();
    loop {
        let stripped = text.trim_start_matches('&').trim_start();
        let stripped = stripped.strip_prefix("mut ").unwrap_or(stripped).trim_start();
        let stripped = stripped.strip_prefix("dyn ").unwrap_or(stripped).trim_start();
        let stripped = if stripped.starts_with('\'') {
            stripped
                .split_once(char::is_whitespace)
                .map_or("", |(_, rest)| rest)
                .trim_start()
        } else {
            stripped
        };
        if stripped == text {
            break;
        }
        text = stripped;
    }
    let head: String = text
        .chars()
        .take_while(|character| is_ident_char(*character) || *character == ':')
        .collect();
    head.rsplit("::").next().unwrap_or(&head).to_string()
}

/// One `let` initialiser in canonical form.
///
/// Whitespace is removed so a multi-line chain reads like a single-line one, and
/// every argument list is reduced to `..` unless it contains another call, in
/// which case the nested call is kept and canonicalised too. The result is the
/// *shape* of the expression: which receiver, which methods, in which order -
/// everything that decides the type, and nothing that does not.
pub(super) fn producer_shape(initialiser: &str) -> String {
    let characters: Vec<char> = initialiser.chars().filter(|c| !c.is_whitespace()).collect();
    let mut shape = String::new();
    let mut index = 0usize;
    while index < characters.len() {
        if characters[index] != '(' {
            shape.push(characters[index]);
            index += 1;
            continue;
        }
        let mut depth = 0usize;
        let mut end = index;
        let mut closed = false;
        while end < characters.len() {
            match characters[end] {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        closed = true;
                        break;
                    }
                }
                _ => {}
            }
            end += 1;
        }
        if !closed {
            // Truncated or unbalanced: nothing can match it, which is the safe
            // direction.
            shape.push_str("(<unbalanced>");
            return shape;
        }
        let inner: String = characters[index + 1..end].iter().collect();
        shape.push('(');
        if inner.contains('(') {
            shape.push_str(&producer_shape(&inner));
        } else if !inner.is_empty() {
            shape.push_str("..");
        }
        shape.push(')');
        index = end + 1;
    }
    shape
}

/// Whether `text` opens with `(` and closes on the matching `)` at its end.
fn is_one_balanced_group(text: &str) -> bool {
    let characters: Vec<char> = text.chars().collect();
    if characters.first() != Some(&'(') {
        return false;
    }
    let mut depth = 0usize;
    for (offset, character) in characters.iter().enumerate() {
        match character {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return offset + 1 == characters.len();
                }
            }
            _ => {}
        }
    }
    false
}

/// Strip the part of a producer shape that cannot change its type: trailing `?`
/// and trailing error-context combinators.
pub(super) fn strip_inert_producer_tail(shape: &str) -> String {
    let mut text = shape;
    loop {
        if let Some(trimmed) = text.strip_suffix('?') {
            text = trimmed;
            continue;
        }
        let mut cut = None;
        for method in PRODUCER_INERT_TAIL_METHODS {
            let Some(position) = text.rfind(method) else {
                continue;
            };
            let arguments = &text[position + method.len() - 1..];
            if is_one_balanced_group(arguments) {
                cut = Some(cut.map_or(position, |current: usize| current.min(position)));
            }
        }
        match cut {
            Some(position) => text = &text[..position],
            None => break,
        }
    }
    text.to_string()
}

/// The non-tool type a producer shape names when it is an associated function
/// of one: `Connection::open(..)`,
/// `crate::tools::ToolExecutionService::from_shared_boxed_registry(..)`.
///
/// The shape has to be that one call and nothing else, so
/// `MessageSendTool::new(..).with_channels(..)` and a chain that turns the
/// result into something else both fall through to review.
pub(super) fn constructor_producer_type(shape: &str) -> Option<&'static str> {
    let (head, arguments) = shape.split_once('(')?;
    if head.contains('.') || !head.contains("::") {
        return None;
    }
    if !is_one_balanced_group(&format!("({arguments}")) {
        return None;
    }
    let mut segments: Vec<&str> = head.split("::").collect();
    segments.pop()?;
    let owner = segments.pop()?;
    NON_TOOL_RECEIVER_TYPES
        .iter()
        .find(|(name, _)| *name == owner)
        .map(|(name, _)| *name)
}

/// The non-`Tool` type one binding proves, if any.
fn non_tool_binding_type(binding: &str) -> Option<&'static str> {
    if let Some(annotation) = binding
        .strip_prefix("parameter: ")
        .or_else(|| binding.strip_prefix("closure parameter: "))
        .or_else(|| binding.strip_prefix("let type: "))
    {
        let base = base_type_identifier(annotation);
        return NON_TOOL_RECEIVER_TYPES
            .iter()
            .find(|(name, _)| *name == base)
            .map(|(name, _)| *name);
    }
    if let Some(callee) = binding.strip_prefix("closure parameter of ") {
        return NON_TOOL_CLOSURE_PROVIDERS
            .iter()
            .find(|(name, parameter, _)| {
                *name == callee && NON_TOOL_RECEIVER_TYPES.iter().any(|(known, _)| known == parameter)
            })
            .map(|(_, parameter, _)| *parameter);
    }
    if let Some(value) = binding.strip_prefix("let value: ") {
        let shape = strip_inert_producer_tail(&producer_shape(value));
        if let Some(owner) = constructor_producer_type(&shape) {
            return Some(owner);
        }
        return NON_TOOL_RECEIVER_PRODUCERS
            .iter()
            .find(|(pattern, _)| *pattern == shape)
            .map(|(pattern, _)| *pattern);
    }
    None
}

/// The non-`Tool` type every binding of the receiver agrees on, or `None` when
/// the receiver has no readable binding or any one of its bindings is not
/// provably a non-`Tool` type.
///
/// "Every binding" is what makes shadowing visible: a function that binds `conn`
/// to a `Connection` and later rebinds it to a `Tool` has one unprovable
/// binding, so both of its call sites return to the inventory.
fn proven_non_tool_receiver(bindings: &[String]) -> Option<String> {
    if bindings.is_empty() {
        return None;
    }
    let mut proofs: Vec<&'static str> = Vec::with_capacity(bindings.len());
    for binding in bindings {
        proofs.push(non_tool_binding_type(binding)?);
    }
    proofs.sort_unstable();
    proofs.dedup();
    Some(proofs.join(", "))
}

/// One `execute(...)` call the scanner found in production source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ToolExecutionSite {
    /// `<file>::<enclosing function>`, the inventory key.
    pub(super) entry: String,
    /// The call form, for the failure message: `tool.execute(...)`, UFCS, and so on.
    pub(super) form: String,
    /// The receiver identifier, empty for a UFCS call that has no receiver.
    pub(super) receiver: String,
    /// The receiver expression as written, e.g. `self.tool_execution`.
    pub(super) receiver_path: String,
    /// Every binding of the receiver identifier inside the enclosing function.
    pub(super) bindings: Vec<String>,
    /// The non-`Tool` type the receiver's own bindings prove, when they do.
    pub(super) non_tool_binding: Option<String>,
    pub(super) line: usize,
}

/// Everything one file's production source says about tool execution.
#[derive(Debug, Default)]
pub(super) struct ToolExecutionScan {
    pub(super) sites: Vec<ToolExecutionSite>,
}

/// Find every direct `Tool` execution in one file's production source.
///
/// Token-level rather than regex-level: comments and literals are blanked first,
/// identifiers are matched whole (so `execute_named` never matches inside
/// `execute_named_with_cancellation`), and the receiver is recovered by walking
/// the expression backwards. That covers the method form, the two cancellation
/// twins, UFCS (`Tool::execute(&tool, args)` and `<T as Tool>::execute(...)`) and
/// call chains broken across lines.
pub(super) fn scan_tool_executions(relative: &str, source: &str) -> ToolExecutionScan {
    let production = strip_cfg_test_items(&blank_comments_and_literals(source));
    let characters: Vec<char> = production.chars().collect();
    let lines: Vec<&str> = production.lines().collect();
    let newlines: Vec<usize> = characters
        .iter()
        .enumerate()
        .filter_map(|(offset, character)| (*character == '\n').then_some(offset))
        .collect();
    let mut scan = ToolExecutionScan::default();

    let mut index = 0usize;
    while index < characters.len() {
        if !is_ident_char(characters[index]) || (index > 0 && is_ident_char(characters[index - 1])) {
            index += 1;
            continue;
        }
        let mut end = index;
        while end < characters.len() && is_ident_char(characters[end]) {
            end += 1;
        }
        let name: String = characters[index..end].iter().collect();
        if !TOOL_EXECUTION_METHODS.contains(&name.as_str()) {
            index = end;
            continue;
        }

        // A call, not a definition or a path mention: the next significant
        // character has to open the argument list.
        let mut after = end;
        while after < characters.len() && characters[after].is_whitespace() {
            after += 1;
        }
        if after >= characters.len() || characters[after] != '(' {
            index = end;
            continue;
        }

        let mut before = index;
        while before > 0 && characters[before - 1].is_whitespace() {
            before -= 1;
        }
        let (receiver, receiver_path, form) = if before > 0 && characters[before - 1] == '.' {
            let path = receiver_expression(&characters, before - 1);
            // Only a receiver that *is* a local binding may be looked up among
            // the function's bindings. `self.conn` and `state.tool` are fields
            // and `checkout.client_mut()?` is a call: a same-named local in the
            // same function says nothing about them, and consulting one is how
            // an audit probe got a `Tool` on `self.conn` waved through by a
            // `conn: &Connection` parameter sitting beside it.
            let receiver = if receiver_is_local_binding(&characters, before - 1) {
                receiver_identifier(&characters, before - 1)
            } else {
                String::new()
            };
            let form = format!("{path}.{name}(...)");
            (receiver, path, form)
        } else if before > 1 && characters[before - 1] == ':' && characters[before - 2] == ':' {
            let segment = qualified_path_segment(&characters, before - 2);
            let form = format!("{segment}::{name}(...)");
            // A UFCS call has no receiver binding to reason about: every
            // `Path::execute(...)` in this tree is a tool execution, and a new
            // non-tool one is a review event rather than something the scanner
            // may guess about.
            (String::new(), String::new(), form)
        } else {
            index = end;
            continue;
        };

        let line = newlines.partition_point(|offset| *offset < index);
        let bindings = if receiver.is_empty() {
            Vec::new()
        } else {
            enclosing_function_body(&lines, line)
                .map(|body| receiver_bindings(&body, &receiver))
                .unwrap_or_default()
        };
        let non_tool_binding = proven_non_tool_receiver(&bindings);
        scan.sites.push(ToolExecutionSite {
            entry: format!("{relative}::{}", enclosing_function(&lines, line)),
            form,
            receiver,
            receiver_path,
            bindings,
            non_tool_binding,
            line: line + 1,
        });
        index = end;
    }
    scan
}

/// Whether `needle` occurs in `haystack` as a whole identifier.
pub(super) fn contains_identifier(haystack: &str, needle: &str) -> bool {
    let characters: Vec<char> = haystack.chars().collect();
    let target: Vec<char> = needle.chars().collect();
    if target.is_empty() || target.len() > characters.len() {
        return false;
    }
    (0..=characters.len() - target.len()).any(|start| {
        characters[start..start + target.len()] == target[..]
            && (start == 0 || !is_ident_char(characters[start - 1]))
            && characters
                .get(start + target.len())
                .is_none_or(|character| !is_ident_char(*character))
    })
}

/// Whether one reviewed exception covers one call site: same file, same
/// enclosing function, same receiver expression.
fn exception_covers(exception: &NonToolCallSite, site: &ToolExecutionSite) -> bool {
    (exception.entry, exception.receiver) == (site.entry.as_str(), site.receiver_path.as_str())
}

/// Count, duplication and staleness checks for the reviewed non-`Tool` call
/// sites.
///
/// A pure function so the rules can be exercised against a synthetic inventory
/// as well as against the real tree.
pub(super) fn non_tool_call_site_errors(sites: &[ToolExecutionSite], exceptions: &[NonToolCallSite]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut seen: BTreeSet<(&str, &str)> = BTreeSet::new();
    for exception in exceptions {
        let label = format!("{} / {}", exception.entry, exception.receiver);
        if !seen.insert((exception.entry, exception.receiver)) {
            errors.push(format!("{label} is listed twice"));
        }
        if exception.receiver.is_empty() {
            errors.push(format!(
                "{label} has no receiver: a UFCS call has no binding to review and is never exempt"
            ));
        }
        if exception.occurrences == 0 {
            errors.push(format!(
                "{label} declares zero occurrences; delete the entry instead of leaving a \
                 standing exemption"
            ));
        }
        if exception.reason.len() <= 20 {
            errors.push(format!(
                "{label} needs a real reason naming the type whose `execute` this is, not {:?}",
                exception.reason
            ));
        }
        if exception.evidence.len() <= 10 {
            errors.push(format!(
                "{label} needs a source fragment that names the receiver's type, not {:?}",
                exception.evidence
            ));
        }
        let actual = sites
            .iter()
            .filter(|site| site.non_tool_binding.is_none() && exception_covers(exception, site))
            .count();
        if actual == 0 {
            errors.push(format!(
                "{label} no longer matches any `execute` call site; remove the entry instead of \
                 leaving a standing exemption"
            ));
        } else if actual != exception.occurrences {
            errors.push(format!(
                "{label} declares {} `execute` call(s) but the tree now has {actual}. Every call \
                 on an exempt receiver is reviewed one at a time: if the new call is another \
                 non-tool call raise the count, and if it is a tool call route it through \
                 ToolExecutionService",
                exception.occurrences
            ));
        }
    }
    errors
}

/// The direct `Tool` executions in a scan: every call whose receiver binding
/// does not prove another type, minus the reviewed non-`Tool` sites.
pub(super) fn direct_tool_execution_sites<'a>(
    sites: &'a [ToolExecutionSite],
    exceptions: &[NonToolCallSite],
) -> Vec<&'a ToolExecutionSite> {
    sites
        .iter()
        .filter(|site| {
            site.non_tool_binding.is_none() && !exceptions.iter().any(|exception| exception_covers(exception, site))
        })
        .collect()
}

pub(super) fn scan_repository_tool_executions() -> ToolExecutionScan {
    let mut combined = ToolExecutionScan::default();
    let test_modules = test_module_files();
    for path in rust_source_files() {
        let relative = relative_path(&path);
        let source = read_source(&path);
        if is_test_code_file(&relative, &source, &test_modules) {
            continue;
        }
        let scan = scan_tool_executions(&relative, &source);
        combined.sites.extend(scan.sites);
    }
    combined
}

/// One inventory line per enclosing function: `<file>::<function> xN`.
///
/// The count is what stops a *second* direct tool execution inside a function
/// that is already allowlisted from disappearing into the deduplicated entry
/// name.
pub(super) fn direct_tool_execution_inventory(sites: &[&ToolExecutionSite]) -> Vec<String> {
    let mut counted: BTreeMap<&str, usize> = BTreeMap::new();
    for site in sites {
        *counted.entry(site.entry.as_str()).or_default() += 1;
    }
    counted
        .into_iter()
        .map(|(entry, count)| format!("{entry} x{count}"))
        .collect()
}
