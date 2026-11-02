//! Read local TPTP metadata and expand include directives without changing formulae.
use regex::Regex;
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::OnceLock,
};

fn includes() -> &'static Regex {
    static INCLUDE: OnceLock<Regex> = OnceLock::new();
    INCLUDE.get_or_init(|| {
        Regex::new(r"(?s)^include\s*\(\s*'([^'\\]+)'\s*(?:,\s*\[([^\]]*)\])?\s*\)\s*\.$")
            .expect("static include pattern")
    })
}

/// Split top-level statements, respecting comments, quoted atoms and decimal points.
pub fn statements(text: &str) -> Result<Vec<String>, String> {
    let mut chars = text.chars().peekable();
    let mut output = Vec::new();
    let mut current = String::new();
    let mut quote = None;
    let mut depth = 0i32;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            current.push(c);
            if c == '\\' {
                if let Some(next) = chars.next() {
                    current.push(next);
                } else {
                    return Err("Unterminated escape".into());
                }
            } else if c == q {
                quote = None;
            }
            continue;
        }
        if c == '%' {
            for next in chars.by_ref() {
                if next == '\n' {
                    break;
                }
            }
            current.push(' ');
            continue;
        }
        if c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut closed = false;
            while let Some(next) = chars.next() {
                if next == '*' && chars.peek() == Some(&'/') {
                    chars.next();
                    closed = true;
                    break;
                }
            }
            if !closed {
                return Err("Unterminated block comment".into());
            }
            current.push(' ');
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            current.push(c);
            continue;
        }
        if c == '(' || c == '[' {
            depth += 1;
        }
        if c == ')' || c == ']' {
            depth -= 1;
            if depth < 0 {
                return Err("Unbalanced statement".into());
            }
        }
        current.push(c);
        if c == '.' && depth == 0 {
            let s = current.trim();
            if !s.is_empty() {
                output.push(s.to_string());
            }
            current.clear();
        }
    }
    if quote.is_some() || depth != 0 || !current.trim().is_empty() {
        return Err("Incomplete TPTP statement".into());
    }
    Ok(output)
}

pub fn contains_include(text: &str) -> Result<bool, String> {
    Ok(statements(text)?.iter().any(|s| {
        s.split('(')
            .next()
            .is_some_and(|head| head.trim() == "include")
    }))
}

fn atom(token: &str) -> String {
    let token = token.trim();
    token
        .strip_prefix('\'')
        .and_then(|s| s.strip_suffix('\''))
        .unwrap_or(token)
        .replace("\\'", "'")
        .replace("\\\\", "\\")
}
fn names(text: &str) -> Result<HashSet<String>, String> {
    let mut result = HashSet::new();
    let mut token = String::new();
    let mut quote = false;
    let mut escape = false;
    for c in text.chars() {
        if escape {
            token.push(c);
            escape = false;
            continue;
        }
        if quote && c == '\\' {
            token.push(c);
            escape = true;
            continue;
        }
        if c == '\'' {
            quote = !quote;
            token.push(c);
        } else if c == ',' && !quote {
            if token.trim().is_empty() {
                return Err("Empty selected formula name".into());
            }
            result.insert(atom(&token));
            token.clear();
        } else {
            token.push(c);
        }
    }
    if quote || escape {
        return Err("Unterminated selected formula name".into());
    }
    if !token.trim().is_empty() {
        result.insert(atom(&token));
    }
    Ok(result)
}
fn formula_name(statement: &str) -> Result<String, String> {
    let tail = statement
        .split_once('(')
        .ok_or("Missing formula arguments")?
        .1;
    let mut quote = false;
    let mut escape = false;
    let mut token = String::new();
    for c in tail.chars() {
        if escape {
            token.push(c);
            escape = false;
            continue;
        }
        if quote && c == '\\' {
            token.push(c);
            escape = true;
            continue;
        }
        if c == '\'' {
            quote = !quote;
        }
        if c == ',' && !quote {
            return Ok(atom(&token));
        }
        token.push(c);
    }
    Err("Missing formula name".into())
}

/// Confine file access to the configured library, including symlink targets.
pub fn confined(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let relative_path = Path::new(relative);
    if relative_path.is_absolute()
        || relative_path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err("Library paths must be relative without traversal".into());
    }
    let path = root
        .join(relative_path)
        .canonicalize()
        .map_err(|e| format!("{relative}: {e}"))?;
    if !path.starts_with(root) {
        return Err("Library path escapes the configured root".into());
    }
    if !path.is_file() {
        return Err("Library entry is not a file".into());
    }
    Ok(path)
}

/// Resolve relative includes against their parent, then the TPTP root.
pub fn expand(root: &Path, id: &str, max_bytes: usize) -> Result<String, String> {
    let root = root.canonicalize().map_err(|e| e.to_string())?;
    let file = confined(&root, id)?;
    let formulas = expand_file(&root, &file, &mut Vec::new(), &mut 0, max_bytes)?;
    let result = formulas
        .into_iter()
        .map(|(_, formula)| formula + "\n")
        .collect::<String>();
    if result.len() > max_bytes {
        return Err("Expanded problem exceeds the input byte limit".into());
    }
    Ok(result)
}
fn expand_file(
    root: &Path,
    file: &Path,
    stack: &mut Vec<PathBuf>,
    used: &mut usize,
    max: usize,
) -> Result<Vec<(String, String)>, String> {
    if stack.len() >= 32 || stack.iter().any(|p| p == file) {
        return Err("Cyclic or excessively deep include chain".into());
    }
    let length = std::fs::metadata(file).map_err(|e| e.to_string())?.len() as usize;
    *used = used.checked_add(length).ok_or("Include size overflow")?;
    if *used > max {
        return Err("Problem and included axioms exceed the input byte limit".into());
    }
    let text = std::fs::read_to_string(file).map_err(|e| e.to_string())?;
    stack.push(file.to_path_buf());
    let mut result = Vec::new();
    for statement in statements(&text)? {
        let head = statement.split('(').next().unwrap_or("").trim();
        if head == "include" {
            let captures = includes()
                .captures(&statement)
                .ok_or("Unsupported include directive")?;
            let relative = &captures[1];
            // Reject traversal before attempting the parent-relative fallback.
            if Path::new(relative)
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err("Invalid include path".into());
            }
            let parent = file.parent().ok_or("Missing parent directory")?;
            let parent_relative = parent
                .strip_prefix(root)
                .map_err(|e| e.to_string())?
                .join(relative);
            let included = confined(root, &parent_relative.to_string_lossy())
                .or_else(|_| confined(root, relative))?;
            let mut nested = expand_file(root, &included, stack, used, max)?;
            if let Some(selection) = captures.get(2) {
                let wanted = names(selection.as_str())?;
                let available: HashSet<_> = nested.iter().map(|(name, _)| name.clone()).collect();
                if !wanted.is_subset(&available) {
                    return Err("Selected include formula was not found".into());
                }
                nested.retain(|(name, _)| wanted.contains(name));
            }
            result.extend(nested);
        } else if [
            "fof", "cnf", "tff", "thf", "tcf", "tpi", "txf", "nxf", "nhf",
        ]
        .contains(&head)
        {
            result.push((formula_name(&statement)?, statement));
        } else {
            return Err(format!("Unsupported TPTP statement: {head}"));
        }
    }
    stack.pop();
    Ok(result)
}

pub fn header(text: &str, key: &str) -> Option<String> {
    text.lines()
        .take_while(|line| line.trim().is_empty() || line.trim_start().starts_with('%'))
        .filter_map(|line| line.trim_start().strip_prefix('%'))
        .find_map(|line| {
            let (field, value) = line.split_once(':')?;
            field
                .trim()
                .eq_ignore_ascii_case(key)
                .then(|| value.trim().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(label: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("atp-library-{label}-{}", std::process::id()));
        std::fs::create_dir_all(root.join("Axioms")).unwrap();
        root
    }
    fn cleanup(root: PathBuf) {
        for entry in std::fs::read_dir(root.join("Axioms")).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(root.join("Axioms")).unwrap();
        for entry in std::fs::read_dir(&root).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn expands_nested_selected_axioms_and_quotes() {
        let root = fixture("selected");
        std::fs::write(
            root.join("Axioms/base.ax"),
            "fof('keep,name',axiom,p). fof(drop,axiom,q).",
        )
        .unwrap();
        std::fs::write(
            root.join("Axioms/nested.ax"),
            "include('Axioms/base.ax',['keep,name']).",
        )
        .unwrap();
        std::fs::write(
            root.join("problem.p"),
            "% include('secret').\ninclude('Axioms/nested.ax'). fof(goal,conjecture,p).",
        )
        .unwrap();
        let text = expand(&root, "problem.p", 10000).unwrap();
        assert!(text.contains("'keep,name'"));
        assert!(!text.contains("drop"));
        assert!(!contains_include(&text).unwrap());
        cleanup(root);
    }
    #[test]
    fn rejects_traversal_cycles_missing_selections_and_limits() {
        let root = fixture("invalid");
        std::fs::write(root.join("Axioms/base.ax"), "fof(a,axiom,p).").unwrap();
        for (input, max) in [
            ("include('../secret').", 10000),
            ("include('problem.p').", 10000),
            ("include('Axioms/base.ax',[missing]).", 10000),
            ("include('Axioms/base.ax').", 4),
        ] {
            std::fs::write(root.join("problem.p"), input).unwrap();
            assert!(expand(&root, "problem.p", max).is_err());
        }
        cleanup(root);
    }
    #[test]
    fn comments_strings_decimals_and_status_headers() {
        let text = "% Status : Theorem\n/* ignored. */ fof(a,axiom, p('include(x).',1.2)).";
        assert_eq!(statements(text).unwrap().len(), 1);
        assert!(!contains_include(text).unwrap());
        assert_eq!(header(text, "Status"), Some("Theorem".into()));
        assert!(statements("fof(a,axiom,p(").is_err());
    }
}
