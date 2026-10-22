//! `routing/scope-match.ts`: allow/exclude list matching over JSON
//! `ScopeLists` (`{allowedModels?, allowedProviders?, excludedModels?,
//! excludedProviders?}`, each a string array). Exact string equality only;
//! excluded wins; an absent/empty allowed list allows all.
use serde_json::Value;

fn list(scope: &Value, key: &str) -> Option<Vec<Value>> {
    match scope.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.clone()),
        // Non-array truthy lists are not produced by the config schema; treat
        // falsy as absent and anything else as a list with no string members.
        Some(v) => {
            if super::super::js::truthy(Some(v)) {
                Some(vec![])
            } else {
                None
            }
        }
    }
}

fn includes(list: &[Value], value: &str) -> bool {
    list.iter().any(|v| v.as_str() == Some(value))
}

/// `listAllowsAny(allowed, excluded, values)`.
pub fn list_allows_any(
    allowed: Option<&[Value]>,
    excluded: Option<&[Value]>,
    values: &[String],
) -> bool {
    if let Some(ex) = excluded {
        if values.iter().any(|v| includes(ex, v)) {
            return false;
        }
    }
    if let Some(al) = allowed {
        if !al.is_empty() && !values.iter().any(|v| includes(al, v)) {
            return false;
        }
    }
    true
}

/// `listAllows(allowed, excluded, value)`.
pub fn list_allows(allowed: Option<&[Value]>, excluded: Option<&[Value]>, value: &str) -> bool {
    list_allows_any(allowed, excluded, &[value.to_owned()])
}

/// `listAllowsAny(scope[allowedKey], scope[excludedKey], values)`.
pub fn scope_allows_any(
    scope: &Value,
    allowed_key: &str,
    excluded_key: &str,
    values: &[String],
) -> bool {
    let allowed = list(scope, allowed_key);
    let excluded = list(scope, excluded_key);
    list_allows_any(allowed.as_deref(), excluded.as_deref(), values)
}

/// `scopeMatches(scope, provider, model)`.
pub fn scope_matches(scope: &Value, provider: &str, model: &str) -> bool {
    scope_allows_any(
        scope,
        "allowedProviders",
        "excludedProviders",
        &[provider.to_owned()],
    ) && scope_allows_any(
        scope,
        "allowedModels",
        "excludedModels",
        &[model.to_owned()],
    )
}

/// `isGlobalScope(scope)`: all four lists absent or empty.
pub fn is_global_scope(scope: &Value) -> bool {
    [
        "allowedModels",
        "allowedProviders",
        "excludedModels",
        "excludedProviders",
    ]
    .iter()
    .all(|k| {
        let v = scope.get(*k);
        !super::super::js::truthy(v) || v.and_then(Value::as_array).is_some_and(Vec::is_empty)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn semantics() {
        let s = json!({"allowedProviders": ["a", "b"], "excludedProviders": ["b"], "allowedModels": []});
        assert!(scope_matches(&s, "a", "anything"));
        assert!(!scope_matches(&s, "b", "m"));
        assert!(!scope_matches(&s, "c", "m"));
        assert!(is_global_scope(&json!({"allowedModels": []})));
        assert!(!is_global_scope(&json!({"excludedModels": ["x"]})));
        assert!(!list_allows_any(
            None,
            Some(&[json!("x@flex")]),
            &["x".into(), "x@flex".into()]
        ));
    }
}
