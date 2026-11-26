//! Reading the RFC 9421 `Signature-Input` and `Signature` header values.
//!
//! Both are RFC 8941 structured-field dictionaries. `Signature-Input` maps a
//! label to an inner list of covered component names plus parameters
//! (`sig1=("@method" "content-digest");created=1;keyid="k"`); `Signature`
//! maps the same labels to base64 byte sequences (`sig1=:aGk=:`). Only the
//! subset RFC 9421 needs is read: a string or token parameter value, an
//! integer, a byte sequence; component identifiers carry no parameters.

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};

use super::super::{MALFORMED_BASE64, SignatureError};

/// How many dictionary members one header may carry before it is refused:
/// a peer signs once, maybe twice, and every member costs a key lookup.
const MAX_MEMBERS: usize = 8;
/// How many covered components one signature may list.
const MAX_COMPONENTS: usize = 32;
/// How many parameters one signature may carry.
const MAX_PARAMS: usize = 16;

/// [`SignatureError::Malformed`] for text that is not a dictionary.
const MALFORMED_FIELD: SignatureError =
    SignatureError::Malformed("not a structured-field dictionary");
/// [`SignatureError::Malformed`] for a covered component with parameters.
const UNSUPPORTED_PARAMS: SignatureError =
    SignatureError::Malformed("covered component parameters are not supported");

/// One `Signature` member: its label and the decoded signature bytes.
pub(super) type LabeledSignature = (String, Vec<u8>);

/// One `Signature-Input` member: a label with its covered components and
/// signature parameters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Input {
    /// The dictionary key that ties this member to a `Signature` member.
    pub(super) label: String,
    /// The covered component names as sent (quotes removed).
    pub(super) components: Vec<String>,
    /// The parameters after the inner list, in the order sent.
    pub(super) params: Vec<Param>,
}

/// One signature parameter: its name and the serialized value as sent.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Param {
    /// The parameter name, e.g. `created`.
    pub(super) name: String,
    /// The value exactly as written (a string keeps its quotes); `None`
    /// for a bare boolean parameter.
    pub(super) raw: Option<String>,
}

impl Input {
    /// The serialized value of the `@signature-params` line: the inner
    /// list and the parameters, re-serialized from what was parsed so that
    /// extra whitespace in the header cannot change the signed text.
    pub(super) fn params_text(&self) -> String {
        let items = self
            .components
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<String>>()
            .join(" ");
        let mut text = format!("({items})");
        for param in &self.params {
            text.push(';');
            text.push_str(&param.name);
            if let Some(raw) = &param.raw {
                text.push('=');
                text.push_str(raw);
            }
        }
        text
    }

    /// The decoded text of string-or-token parameter `name`.
    pub(super) fn text_param(&self, name: &str) -> Option<String> {
        let raw = self.raw_param(name)?;
        if raw.starts_with('"') {
            unquote(raw)
        } else {
            Some(raw.to_owned())
        }
    }

    /// Integer parameter `name`; `Ok(None)` when it is absent.
    pub(super) fn int_param(&self, name: &str) -> Result<Option<i64>, SignatureError> {
        self.raw_param(name)
            .map(|raw| {
                raw.parse::<i64>().map_err(|_err| {
                    SignatureError::Malformed("a timestamp parameter that is not an integer")
                })
            })
            .transpose()
    }

    /// The serialized value of parameter `name`, the first one if repeated.
    fn raw_param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|param| param.name == name)
            .and_then(|param| param.raw.as_deref())
    }
}

/// Reads a `Signature-Input` header value.
///
/// # Errors
///
/// [`SignatureError::Malformed`] when the text is not a dictionary of inner
/// lists, a covered component carries parameters, or a limit is exceeded.
pub(super) fn parse_signature_input(text: &str) -> Result<Vec<Input>, SignatureError> {
    members(text, |cursor, label| {
        let components = inner_list(cursor)?;
        let params = parameters(cursor)?;
        Ok(Input {
            label,
            components,
            params,
        })
    })
}

/// Reads a `Signature` header value in its RFC 9421 form: label to raw
/// signature bytes.
///
/// # Errors
///
/// [`SignatureError::Malformed`] when the text is not a dictionary of byte
/// sequences or one of them is not base64.
pub(super) fn parse_signatures(text: &str) -> Result<Vec<LabeledSignature>, SignatureError> {
    members(text, |cursor, label| {
        let encoded = byte_sequence(cursor)?;
        let signature = BASE64.decode(encoded).map_err(|_err| MALFORMED_BASE64)?;
        drop(parameters(cursor)?);
        Ok((label, signature))
    })
}

/// A cursor over the not yet consumed text of a header value.
struct Cursor<'a> {
    /// What is left to read.
    rest: &'a str,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `text`.
    const fn new(text: &'a str) -> Self {
        Self { rest: text }
    }

    /// Consumes `expected` when it is next; whether it was.
    fn eat(&mut self, expected: char) -> bool {
        match self.rest.strip_prefix(expected) {
            Some(tail) => {
                self.rest = tail;
                true
            }
            None => false,
        }
    }

    /// The next character, not consumed.
    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    /// Consumes and returns the longest prefix whose characters satisfy `keep`.
    fn take_while<F: Fn(char) -> bool>(&mut self, keep: F) -> &'a str {
        let end = self
            .rest
            .find(|c: char| !keep(c))
            .unwrap_or(self.rest.len());
        let (head, tail) = self.rest.split_at_checked(end).unwrap_or((self.rest, ""));
        self.rest = tail;
        head
    }

    /// Skips spaces and tabs.
    fn skip_blanks(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t']);
    }
}

/// Reads the members of a dictionary, handing each value to `member`.
fn members<T, F>(text: &str, mut member: F) -> Result<Vec<T>, SignatureError>
where
    F: FnMut(&mut Cursor<'_>, String) -> Result<T, SignatureError>,
{
    let mut cursor = Cursor::new(text);
    let mut found = Vec::new();
    cursor.skip_blanks();
    while !cursor.rest.is_empty() {
        let label = key(&mut cursor)?;
        if !cursor.eat('=') {
            return Err(MALFORMED_FIELD);
        }
        found.push(member(&mut cursor, label)?);
        if found.len() > MAX_MEMBERS {
            return Err(MALFORMED_FIELD);
        }
        cursor.skip_blanks();
        if cursor.rest.is_empty() {
            break;
        }
        if !cursor.eat(',') {
            return Err(MALFORMED_FIELD);
        }
        cursor.skip_blanks();
        if cursor.rest.is_empty() {
            return Err(MALFORMED_FIELD);
        }
    }
    Ok(found)
}

/// A dictionary key or parameter name: lowercase letters, digits and
/// `_ - . *`, starting with a letter or `*`.
fn key(cursor: &mut Cursor<'_>) -> Result<String, SignatureError> {
    if !cursor
        .peek()
        .is_some_and(|c| c.is_ascii_lowercase() || c == '*')
    {
        return Err(MALFORMED_FIELD);
    }
    let name =
        cursor.take_while(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "_-.*".contains(c));
    Ok(name.to_owned())
}

/// The covered components: `( "name" "name" ... )`, each a bare string.
fn inner_list(cursor: &mut Cursor<'_>) -> Result<Vec<String>, SignatureError> {
    if !cursor.eat('(') {
        return Err(MALFORMED_FIELD);
    }
    let mut names = Vec::new();
    loop {
        cursor.rest = cursor.rest.trim_start_matches(' ');
        if cursor.eat(')') {
            return Ok(names);
        }
        let (_raw, name) = quoted(cursor)?;
        if cursor.peek() == Some(';') {
            return Err(UNSUPPORTED_PARAMS);
        }
        names.push(name);
        if names.len() > MAX_COMPONENTS || !matches!(cursor.peek(), Some(' ' | ')')) {
            return Err(MALFORMED_FIELD);
        }
    }
}

/// The `;name` and `;name=value` parameters at the cursor.
fn parameters(cursor: &mut Cursor<'_>) -> Result<Vec<Param>, SignatureError> {
    let mut params = Vec::new();
    while cursor.eat(';') {
        cursor.rest = cursor.rest.trim_start_matches(' ');
        let name = key(cursor)?;
        let raw = if cursor.eat('=') {
            Some(bare_item(cursor)?)
        } else {
            None
        };
        params.push(Param { name, raw });
        if params.len() > MAX_PARAMS {
            return Err(MALFORMED_FIELD);
        }
    }
    Ok(params)
}

/// The serialized text of one bare item: a string, a byte sequence, or an
/// integer/token running to the next separator.
fn bare_item(cursor: &mut Cursor<'_>) -> Result<String, SignatureError> {
    match cursor.peek() {
        Some('"') => quoted(cursor).map(|(raw, _decoded)| raw.to_owned()),
        Some(':') => byte_sequence(cursor).map(|encoded| format!(":{encoded}:")),
        Some(_) => {
            let raw = cursor.take_while(|c| !matches!(c, ';' | ',' | ' ' | '\t' | ')'));
            if raw.is_empty() {
                Err(MALFORMED_FIELD)
            } else {
                Ok(raw.to_owned())
            }
        }
        None => Err(MALFORMED_FIELD),
    }
}

/// The base64 text between the colons of a byte sequence.
fn byte_sequence<'a>(cursor: &mut Cursor<'a>) -> Result<&'a str, SignatureError> {
    if !cursor.eat(':') {
        return Err(MALFORMED_FIELD);
    }
    let encoded = cursor.take_while(|c| c != ':');
    if cursor.eat(':') {
        Ok(encoded)
    } else {
        Err(MALFORMED_FIELD)
    }
}

/// One quoted string: the raw text with its quotes and the unescaped
/// content (`\"` and `\\` are the only escapes).
fn quoted<'a>(cursor: &mut Cursor<'a>) -> Result<(&'a str, String), SignatureError> {
    let start = cursor.rest;
    let body = start.strip_prefix('"').ok_or(MALFORMED_FIELD)?;
    let mut decoded = String::new();
    let mut escaped = false;
    for (index, c) in body.char_indices() {
        if escaped {
            if !matches!(c, '"' | '\\') {
                return Err(MALFORMED_FIELD);
            }
            decoded.push(c);
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '"' {
            let end = index + 2;
            let raw = start.get(..end).ok_or(MALFORMED_FIELD)?;
            cursor.rest = start.get(end..).ok_or(MALFORMED_FIELD)?;
            return Ok((raw, decoded));
        } else if c.is_ascii_control() {
            return Err(MALFORMED_FIELD);
        } else {
            decoded.push(c);
        }
    }
    Err(MALFORMED_FIELD)
}

/// The content of a serialized string that is nothing else.
fn unquote(raw: &str) -> Option<String> {
    let mut cursor = Cursor::new(raw);
    let parsed = quoted(&mut cursor).ok()?;
    cursor.rest.is_empty().then_some(parsed.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_signature_input_dictionary_parses_with_its_parameters() {
        let parsed = parse_signature_input(
            r#"sig1=("@method" "@target-uri" "content-digest");created=1618884473;keyid="test-key";alg="rsa-v1_5-sha256", sig2=("@method");created=5"#,
        )
        .unwrap();
        assert_eq!(parsed.len(), 2, "two labels");
        let first = parsed.first().unwrap();
        assert_eq!(first.label, "sig1", "the label");
        assert_eq!(
            first.components,
            ["@method", "@target-uri", "content-digest"].map(str::to_owned),
            "components in order"
        );
        assert_eq!(
            first.int_param("created"),
            Ok(Some(1_618_884_473)),
            "created"
        );
        assert_eq!(
            first.text_param("keyid").as_deref(),
            Some("test-key"),
            "keyid"
        );
        assert_eq!(
            first.text_param("alg").as_deref(),
            Some("rsa-v1_5-sha256"),
            "alg"
        );
        assert_eq!(first.int_param("expires"), Ok(None), "absent expires");
        assert_eq!(
            first.params_text(),
            r#"("@method" "@target-uri" "content-digest");created=1618884473;keyid="test-key";alg="rsa-v1_5-sha256""#,
            "the params line is the member re-serialized"
        );
    }

    #[test]
    fn extra_whitespace_does_not_change_the_params_text() {
        let tidy = parse_signature_input(r#"s=("@method" "host");created=1"#).unwrap();
        let loose = parse_signature_input(r#"  s=(  "@method"   "host" );  created=1  "#).unwrap();
        assert_eq!(
            tidy.first().unwrap().params_text(),
            loose.first().unwrap().params_text(),
            "spacing is not part of the signed text"
        );
    }

    #[test]
    fn string_escapes_and_commas_inside_values_are_kept() {
        let parsed = parse_signature_input(r#"s=("@method");keyid="a,b\"c\\d";nonce="x""#).unwrap();
        assert_eq!(
            parsed.first().unwrap().text_param("keyid").as_deref(),
            Some(r#"a,b"c\d"#),
            "a comma does not split and escapes decode"
        );
    }

    #[test]
    fn a_signature_dictionary_decodes_its_byte_sequences() {
        let parsed = parse_signatures("sig1=:aGk=:, sig2=:aGk=:;x=1").unwrap();
        assert_eq!(
            parsed,
            vec![
                ("sig1".to_owned(), b"hi".to_vec()),
                ("sig2".to_owned(), b"hi".to_vec())
            ],
            "both members decode"
        );
    }

    #[test]
    fn malformed_values_are_refused() {
        for text in [
            "garbage",
            "sig1",
            "sig1=\"@method\"",
            r#"sig1=("@method"#,
            r#"sig1=("@method";req);created=1"#,
            r#"sig1=("@method"),"#,
            r#"sig1=("@method");created="#,
            r#"sig1=("@method")sig2=("host")"#,
            r#"Sig1=("@method")"#,
        ] {
            assert!(
                parse_signature_input(text).is_err(),
                "`{text}` must not parse as a Signature-Input"
            );
        }
        for text in [
            "sig1=:not base64!:",
            "sig1=:aGk=",
            "sig1=aGk=",
            "sig1=:aGk=:,",
        ] {
            assert!(
                parse_signatures(text).is_err(),
                "`{text}` must not parse as a Signature"
            );
        }
    }

    #[test]
    fn too_many_members_are_refused() {
        let many = (0..=MAX_MEMBERS)
            .map(|index| format!(r#"s{index}=("@method")"#))
            .collect::<Vec<String>>()
            .join(",");
        assert_eq!(
            parse_signature_input(&many),
            Err(MALFORMED_FIELD),
            "one more member than the cap"
        );
    }
}
