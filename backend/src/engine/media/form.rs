//! A `FormData` model: ordered parts with WHATWG `append` / `set` semantics,
//! sent as `multipart/form-data` like Bun's `fetch`.
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum FormValue {
    Text(String),
    /// A `Blob`/`File` part: bytes, content type (`""` → none) and filename.
    Blob {
        data: Vec<u8>,
        mime: String,
        filename: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct FormPart {
    pub name: String,
    pub value: FormValue,
}

impl FormPart {
    pub fn text(name: &str, value: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            value: FormValue::Text(value.into()),
        }
    }
    pub fn blob(name: &str, data: Vec<u8>, mime: &str, filename: &str) -> Self {
        Self {
            name: name.into(),
            value: FormValue::Blob {
                data,
                mime: mime.into(),
                filename: filename.into(),
            },
        }
    }
}

/// `FormData.set(key, value)`: replaces the first entry named `key` (dropping
/// later duplicates) or appends.
pub fn form_set(parts: &mut Vec<FormPart>, key: &str, value: String) {
    match parts.iter().position(|p| p.name == key) {
        Some(i) => {
            parts[i].value = FormValue::Text(value);
            let mut seen = 0;
            parts.retain(|p| {
                if p.name != key {
                    return true;
                }
                seen += 1;
                seen == 1
            });
        }
        None => parts.push(FormPart::text(key, value)),
    }
}

/// An upstream image payload: JSON or multipart.
#[derive(Debug, Clone, PartialEq)]
pub enum ImagePayload {
    Json(Value),
    Form(Vec<FormPart>),
}

/// reqwest multipart form for the parts (Bun omits `Content-Type` on text
/// parts and sends the Blob type, `application/octet-stream` when empty).
pub fn to_multipart(parts: &[FormPart]) -> reqwest::multipart::Form {
    let mut form = reqwest::multipart::Form::new();
    for part in parts {
        form = match &part.value {
            FormValue::Text(t) => form.text(part.name.clone(), t.clone()),
            FormValue::Blob {
                data,
                mime,
                filename,
            } => {
                // Bun normalizes a `text/plain` Blob type with a charset.
                let mime = match mime.as_str() {
                    "" => "application/octet-stream",
                    "text/plain" => "text/plain;charset=utf-8",
                    m => m,
                };
                let p = reqwest::multipart::Part::bytes(data.clone()).file_name(filename.clone());
                let p = p.mime_str(mime).unwrap_or_else(|_| {
                    reqwest::multipart::Part::bytes(data.clone()).file_name(filename.clone())
                });
                form.part(part.name.clone(), p)
            }
        };
    }
    form
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn set_replaces_first_and_drops_duplicates() {
        let mut parts = vec![
            FormPart::text("a", "1"),
            FormPart::text("b", "2"),
            FormPart::text("a", "3"),
        ];
        form_set(&mut parts, "a", "9".into());
        assert_eq!(
            parts,
            vec![FormPart::text("a", "9"), FormPart::text("b", "2")]
        );
        form_set(&mut parts, "c", "x".into());
        assert_eq!(parts.last().unwrap(), &FormPart::text("c", "x"));
    }
}
