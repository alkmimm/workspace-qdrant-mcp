//! Core recursive-descent parsing logic for regex literal extraction.

use super::super::types::RegexLiterals;

/// Core extraction logic, separated for recursion on group contents.
pub(super) fn extract_literals_recursive(pattern: &str, result: &mut RegexLiterals) {
    // A top-level alternation (`a|b|c`, no enclosing group) must be treated
    // exactly like the group form `(a|b|c)`: every branch collapses into ONE
    // OR'd alternation group. Route it through the shared group path so the
    // branch handling is identical. This fixes the previous binary-split logic,
    // where 3+ top-level branches produced multiple alternation groups that
    // `build_fts5_query` then AND'd together — yielding an impossible
    // `("b" OR "c") AND "a"` candidate query that matched zero rows.
    if split_alternation(pattern).len() > 1 {
        process_group_with_affixes("", "", pattern, result);
        return;
    }

    let mut current = String::new();
    let mut chars = pattern.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                // Escape sequence
                if let Some(&next) = chars.peek() {
                    match next {
                        // Metacharacter classes — end the current literal run
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z' | 'G' => {
                            flush_to_mandatory(&mut current, &mut result.mandatory);
                            chars.next();
                        }
                        // Escaped literals — add the literal character
                        _ => {
                            chars.next();
                            current.push(next);
                        }
                    }
                }
            }
            // Character class — skip everything until closing `]`
            '[' => {
                flush_to_mandatory(&mut current, &mut result.mandatory);
                while let Some(inner) = chars.next() {
                    if inner == '\\' {
                        chars.next();
                    } else if inner == ']' {
                        break;
                    }
                }
            }
            // Group start — extract group content, check for alternation
            '(' => {
                let prefix = std::mem::take(&mut current);
                if prefix.len() >= 3 {
                    result.mandatory.push(prefix.clone());
                }
                let group_content = extract_group_content(&mut chars);
                let suffix = collect_literal_suffix(&mut chars);
                process_group_with_affixes(&prefix, &suffix, &group_content, result);
                if suffix.len() >= 3 {
                    result.mandatory.push(suffix);
                }
            }
            // Unreachable in practice: any depth-0 `|` makes `split_alternation`
            // return >1 branch, so top-level alternations are handled by the
            // guard at the top of this function before the walk begins, and a
            // `|` inside a group or character class is consumed by their
            // handlers. Kept defensive: treat a stray `|` as a run terminator
            // rather than reviving the broken binary-split.
            '|' => {
                flush_to_mandatory(&mut current, &mut result.mandatory);
            }
            // Other metacharacters that end a literal run
            '.' | '*' | '+' | '?' | ']' | ')' | '{' | '}' | '^' | '$' => {
                flush_to_mandatory(&mut current, &mut result.mandatory);
            }
            // Literal character
            _ => {
                current.push(ch);
            }
        }
    }

    flush_to_mandatory(&mut current, &mut result.mandatory);
}

/// Extract the content of a parenthesized group, handling nested parens.
pub(super) fn extract_group_content(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> String {
    let mut content = String::new();
    let mut depth = 1;
    while let Some(ch) = chars.next() {
        match ch {
            '(' => {
                depth += 1;
                content.push(ch);
            }
            ')' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
                content.push(ch);
            }
            '\\' => {
                content.push(ch);
                if let Some(next) = chars.next() {
                    content.push(next);
                }
            }
            _ => content.push(ch),
        }
    }
    content
}

/// Collect literal characters immediately following a group close `)`.
pub(super) fn collect_literal_suffix(
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
) -> String {
    let mut suffix = String::new();
    while let Some(&ch) = chars.peek() {
        match ch {
            '\\' => {
                let mut lookahead = chars.clone();
                lookahead.next();
                if let Some(&next) = lookahead.peek() {
                    match next {
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z' | 'G' => {
                            break
                        }
                        _ => {
                            chars.next();
                            chars.next();
                            suffix.push(next);
                        }
                    }
                } else {
                    break;
                }
            }
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$' => {
                break;
            }
            _ => {
                suffix.push(ch);
                chars.next();
            }
        }
    }
    suffix
}

/// Process a group's content with optional prefix/suffix affixes.
pub(super) fn process_group_with_affixes(
    prefix: &str,
    suffix: &str,
    content: &str,
    result: &mut RegexLiterals,
) {
    let branches = split_alternation(content);
    if branches.len() <= 1 {
        extract_literals_recursive(content, result);
    } else {
        let mut alt_group: Vec<String> = Vec::new();
        for branch in &branches {
            let mut branch_result = RegexLiterals {
                mandatory: Vec::new(),
                alternations: Vec::new(),
            };
            extract_literals_recursive(branch, &mut branch_result);
            if branch_result.mandatory.is_empty() {
                if branch_result.alternations.is_empty() {
                    // Plain literal branch.
                    let combined = format!("{}{}{}", prefix, branch, suffix);
                    if combined.len() >= 3 && is_all_literal(branch) {
                        alt_group.push(combined);
                    }
                } else {
                    // Branch is itself a (possibly nested) pure alternation, e.g.
                    // `a|(b|c)`. Its sub-branches are OR-siblings of THIS group,
                    // so merge them in (with affixes) rather than emitting them as
                    // a separate group that build_fts5_query would wrongly AND —
                    // the same zero-candidate failure the flat case had.
                    for sub in branch_result.alternations.drain(..) {
                        for term in sub {
                            let combined = format!("{}{}{}", prefix, term, suffix);
                            if combined.len() >= 3 {
                                alt_group.push(combined);
                            }
                        }
                    }
                }
            } else {
                for lit in &branch_result.mandatory {
                    let combined = format!("{}{}{}", prefix, lit, suffix);
                    if combined.len() >= 3 {
                        alt_group.push(combined);
                    } else if lit.len() >= 3 {
                        alt_group.push(lit.clone());
                    }
                }
                // Rare mixed case (branch has mandatory literals AND nested
                // alternations, e.g. `foo.*(a|b)`): the flat literal model can't
                // represent `mandatory AND (a OR b)` as one OR-sibling, so surface
                // the sub-alternations as their own groups (prior behavior).
                result
                    .alternations
                    .extend(std::mem::take(&mut branch_result.alternations));
            }
        }
        if !alt_group.is_empty() {
            result.alternations.push(alt_group);
        }
    }
}

/// Split a group's content by top-level `|` (respecting nested parens).
fn split_alternation(content: &str) -> Vec<String> {
    let mut branches = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    let mut chars = content.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth -= 1;
                current.push(ch);
            }
            '\\' => {
                current.push(ch);
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            // Character class: copy verbatim through the closing `]` so a literal
            // `|` inside `[...]` is never treated as an alternation separator.
            '[' => {
                current.push(ch);
                while let Some(inner) = chars.next() {
                    current.push(inner);
                    if inner == '\\' {
                        if let Some(esc) = chars.next() {
                            current.push(esc);
                        }
                    } else if inner == ']' {
                        break;
                    }
                }
            }
            '|' if depth == 0 => {
                branches.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    branches.push(current);
    branches
}

/// Check if a string contains only literal characters (no regex metacharacters).
fn is_all_literal(s: &str) -> bool {
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                if let Some(next) = chars.next() {
                    match next {
                        'd' | 'D' | 'w' | 'W' | 's' | 'S' | 'b' | 'B' | 'A' | 'z' | 'Z' | 'G' => {
                            return false
                        }
                        _ => {}
                    }
                }
            }
            '.' | '*' | '+' | '?' | '[' | ']' | '(' | ')' | '{' | '}' | '|' | '^' | '$' => {
                return false;
            }
            _ => {}
        }
    }
    true
}

/// Flush the current literal buffer into the mandatory list if >= 3 chars.
pub(super) fn flush_to_mandatory(current: &mut String, mandatory: &mut Vec<String>) {
    if current.len() >= 3 {
        mandatory.push(current.clone());
    }
    current.clear();
}
