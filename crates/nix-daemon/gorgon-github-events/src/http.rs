// SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
// SPDX-License-Identifier: EUPL-1.2+

use std::borrow::Cow;

use tracing::warn;
use url::Url;

fn consume_leading_ws(s: &mut &[u8]) {
    while let Some((b' ' | b'\t', rest)) = s.split_first() {
        *s = rest;
    }
}

// RFC 2822 Appendix B.4.
fn parse_quoted_string(s: &mut &[u8]) -> Vec<u8> {
    let mut output = Vec::new(); // 1.

    if let Some((b'"', rest)) = s.split_first() {
        *s = rest; // 3.

        // 4.
        loop {
            match s.split_first() {
                None => break,

                Some((b'\\', rest)) => {
                    // 4.1.
                    *s = rest; // 4.1.1.

                    if let Some((&c, rest)) = s.split_first() {
                        // 4.1.3.
                        output.push(c);
                        *s = rest;
                    } else {
                        break; // 4.1.2.
                    }
                }

                // 4.2.
                Some((b'"', rest)) => {
                    *s = rest;
                    break;
                }

                // 4.3.
                Some((&c, rest)) => {
                    output.push(c);
                    *s = rest;
                }
            }
        }
    }

    // 2. / 5.
    output
}

fn resolve_uri_relative_to(uri: &str, base: &Url) -> Result<Url, url::ParseError> {
    Url::options().base_url(Some(base)).parse(uri)
}

/// Returns the value of the "rel" parameter ("rel*" is not supported).
// Adapted from RFC 8288 Appendix B.3.
fn parse_link_parameters<'a>(input: &mut &'a [u8]) -> Cow<'a, [u8]> {
    let mut rel = Cow::Borrowed(&b""[..]);

    loop {
        consume_leading_ws(input);

        if let Some((b';', rest)) = input.split_first() {
            *input = rest;
        } else {
            // The RFC doesn't actually say to consume the comma,
            // but not doing so is nonsensical as it would break
            // the next iteration of parse_link_field_value.
            if let Some((b',', rest)) = input.split_first() {
                *input = rest;
            }

            return rel;
        }

        consume_leading_ws(input);

        let index = input
            .iter()
            .position(|b| b" \t=;,".contains(b))
            .unwrap_or(input.len());
        let parameter_name;
        (parameter_name, *input) = input.split_at(index);

        consume_leading_ws(input);

        let mut parameter_value: Cow<[u8]> = Cow::Borrowed(&[]);
        if let Some((b'=', rest)) = input.split_first() {
            *input = rest;
            consume_leading_ws(input);

            if input.first() == Some(&b'"') {
                parameter_value = Cow::Owned(parse_quoted_string(input));
            } else {
                let end = input
                    .iter()
                    .position(|b| b";,".contains(b))
                    .unwrap_or(input.len());
                let parameter_value_slice;
                (parameter_value_slice, *input) = input.split_at(end);
                parameter_value = Cow::Borrowed(parameter_value_slice);
            }
        }

        if parameter_name.eq_ignore_ascii_case(b"rel") {
            rel = parameter_value;
        }

        consume_leading_ws(input);
    }
}

/// Deliberately does not support anchor parameters, as permitted by RFC 2822.
// Adapted from RFC 8288 Appendex B.2.
pub fn get_link(rel: &[u8], field_value: &mut &[u8], uri: &Url) -> Option<Url> {
    while !field_value.is_empty() {
        consume_leading_ws(field_value);

        if let Some((b'<', rest)) = field_value.split_first() {
            *field_value = rest;
        } else {
            if !field_value.is_empty() {
                warn!("No '<' in {:?}", String::from_utf8_lossy(field_value));
            }
            break;
        };

        let mut iter = field_value.splitn(2, |&b| b == b'>');
        let target_string_bytes = iter.next().unwrap_or(b"");
        let Ok(target_string) = std::str::from_utf8(target_string_bytes) else {
            warn!(
                "target is not UTF-8: {:?}",
                String::from_utf8_lossy(target_string_bytes)
            );
            break;
        };

        if let Some(rest) = iter.next() {
            *field_value = rest;
        } else {
            warn!("no parameters for {:?}", target_string);
            break;
        }

        let mut relations_string: &[_] = &parse_link_parameters(field_value);

        let Ok(target_uri) = resolve_uri_relative_to(target_string, uri) else {
            warn!(
                "failed to resolve {:?} relative to {:?}",
                target_string, uri
            );
            break;
        };

        loop {
            if relations_string.starts_with(rel) {
                if let Some(b' ' | b'\t') | None = relations_string.get(rel.len()) {
                    return Some(target_uri);
                }
            }

            let Some(pos) = relations_string.iter().position(|b| b" \t".contains(b)) else {
                break;
            };
            relations_string = &relations_string[pos..];
            consume_leading_ws(&mut relations_string);
        }
    }

    None
}
